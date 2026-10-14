//! The scope registry's projection of concierge — the consumer of `ScopeEvents.PullScopeEvents`.
//!
//! `scopes` (migration 0052) is what admission reads when a user moves money into a scope's
//! compartment; until this consumer it held only what 0052 seeded, so a scope concierge
//! froze, closed or registered never reached the money plane. One background task pulls the
//! stream with the shared bridge token — the unfiltered reader, every scope — and applies
//! each SCOPE_CHANGED snapshot to `scopes`. GRANTS_CHANGED and MEMBER_JOINED are consumed
//! and dropped: a service's rights here are banking's own configuration, never concierge's
//! grants, and users arrive over the lifecycle bridge.
//!
//! At-least-once delivery is absorbed by the scope's `version`: a snapshot applies only when
//! it is newer than the row, so a redelivery is a no-op and an older snapshot can never undo
//! a newer FROZEN. The cursor (`scope_events_cursor`) advances only after the whole batch was
//! applied or parked.
//!
//! WHAT THIS BUILD CANNOT READ IS PARKED, NOT HELD. The lifecycle bridge stops its cursor on
//! an unnameable kind because a swallowed SUSPENDED would leave a user's money moving; here
//! the cost runs the other way — one unreadable row at the head would hold back every other
//! scope's freeze until a deploy. So the event is recorded by its position in
//! `scope_events_parked`, the cursor moves on, and every start re-pulls each parked position
//! alone (`after = position - 1, limit = 1`) and applies it once the build can read it. A
//! SCOPE_CHANGED whose STATE this build cannot name is the one case that also acts at once:
//! the scope stops taking money in (`frozen`) until a build that knows the state replays it,
//! because guessing ACTIVE is how an unknown restriction would be ignored on the money plane.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use domain::scopes::{ScopeId, ScopeState};
use evconcierge_contracts::concierge::v1::{PullScopeEventsRequest, Scope, ScopeEvent, ScopeState as WireScopeState, scope_event::Kind, scope_events_client::ScopeEventsClient};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tonic::{Request, metadata::MetadataValue, transport::Channel};
use tracing::{error, info, warn};

/// Rows requested per pull; concierge caps it at its own ceiling.
const PULL_LIMIT: u32 = 256;

/// Ceiling on the retry delay after consecutive stalled cycles — the lifecycle bridge's,
/// for the same reasons (`bridge::BACKOFF_MAX`).
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// `scopes.display_name`'s CHECK. concierge caps it at 80 bytes, so this only ever trips on
/// a contract change; the id stands in rather than the event being refused.
const MAX_DISPLAY_NAME_CHARS: usize = 128;

/// What [`ScopeEventsConsumer::apply`] did with one event. Every outcome lets the cursor
/// pass; the distinction is what gets logged and whether a parked row may be dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
	/// The snapshot changed the row (or registered the scope).
	Applied,
	/// Nothing to do: a redelivery, an older snapshot, or a kind banking does not act on.
	Ignored,
	/// Recorded in `scope_events_parked` for a build that can read it.
	Parked,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Pull {
	progressed: bool,
	more: bool,
	/// Events came back and the cursor could not move past them — see the lifecycle bridge's
	/// `Pull::wedged`.
	wedged: bool,
}

/// The scope-stream consumer task: pull → apply → advance the cursor, on a poll interval.
pub struct ScopeEventsConsumer {
	pool: PgPool,
	channel: Channel,
	service_token: String,
	poll_interval: Duration,
}

impl ScopeEventsConsumer {
	pub fn new(pool: PgPool, channel: Channel, service_token: String, poll_interval: Duration) -> Self {
		Self {
			pool,
			channel,
			service_token,
			poll_interval,
		}
	}

	/// Run until `shutdown` is cancelled. A cycle that had work and got none of it done backs
	/// off exponentially up to [`BACKOFF_MAX`]; an idle or progressing cycle waits the poll
	/// interval — the lifecycle bridge's rule, argued there (`BridgeConsumer::run`).
	///
	/// The parked rows are replayed once per process, before the first pull: what parks them
	/// is this build not knowing something, so only a new build can resolve one. A replay
	/// that fails (concierge down at boot) is retried every cycle until one pass completes.
	pub async fn run(self, shutdown: CancellationToken) {
		info!(every = ?self.poll_interval, "scope events: consuming concierge's scope stream");
		let mut client = ScopeEventsClient::new(self.channel.clone());
		let ceiling = BACKOFF_MAX.max(self.poll_interval);
		let mut backoff: Option<Duration> = None;
		let mut replayed = false;
		loop {
			let mut cycle = Pull::default();
			let failure = async {
				if !replayed {
					self.replay_parked(&mut client).await?;
					replayed = true;
				}
				self.drain(&mut client, &mut cycle).await
			}
			.await
			.err();
			let stalled = !cycle.progressed && (failure.is_some() || cycle.wedged);
			let wait = if stalled {
				let next = backoff.map_or(self.poll_interval, |previous| previous.saturating_mul(2).min(ceiling));
				backoff = Some(next);
				next
			} else {
				backoff = None;
				self.poll_interval
			};
			if let Some(err) = failure {
				let hint = match err.downcast_ref::<tonic::Status>().map(tonic::Status::code) {
					Some(tonic::Code::Unavailable) => " (is concierge running?)",
					// A concierge older than the scope stream answers every pull this way; the
					// registry keeps its last state until one that serves it is deployed.
					Some(tonic::Code::Unimplemented) => " (concierge predates ScopeEvents)",
					_ => "",
				};
				warn!(retry_in = ?wait, progressed = cycle.progressed, "scope events: pull/apply cycle failed, retrying{hint}: {err}");
			}
			tokio::select! {
				() = shutdown.cancelled() => {
					info!("scope events: shutdown requested — stopping");
					return;
				},
				() = tokio::time::sleep(wait) => {},
			}
		}
	}

	/// Pull and apply until a short batch says the stream is drained. `cycle` survives the
	/// `?` so [`Self::run`] knows what was committed before an error cut the drain short.
	async fn drain(&self, client: &mut ScopeEventsClient<Channel>, cycle: &mut Pull) -> color_eyre::Result<()> {
		loop {
			let pull = self.pull_and_apply(client).await?;
			cycle.progressed |= pull.progressed;
			cycle.wedged |= pull.wedged;
			if !pull.more {
				return Ok(());
			}
		}
	}

	fn request(&self, after_position: i64, limit: u32) -> color_eyre::Result<Request<PullScopeEventsRequest>> {
		let mut request = Request::new(PullScopeEventsRequest { after_position, limit });
		let token: MetadataValue<_> = format!("Bearer {}", self.service_token).parse()?;
		request.metadata_mut().insert("authorization", token);
		Ok(request)
	}

	async fn pull_and_apply(&self, client: &mut ScopeEventsClient<Channel>) -> color_eyre::Result<Pull> {
		let after = self.cursor().await?;
		let response = client.pull_scope_events(self.request(after, PULL_LIMIT)?).await?.into_inner();
		if response.events.is_empty() {
			// The bridge token reads unfiltered, so an empty page ends where it began; a page
			// that scanned past rows it was not shown still moves the cursor over them.
			if response.next_position > after {
				self.advance_cursor(after, response.next_position).await?;
				return Ok(Pull {
					progressed: true,
					..Pull::default()
				});
			}
			return Ok(Pull::default());
		}
		let (mut applied, mut parked) = (0usize, 0usize);
		for event in &response.events {
			match self.apply(event).await? {
				Outcome::Applied => applied += 1,
				Outcome::Parked => parked += 1,
				Outcome::Ignored => {}
			}
		}
		// Ending here instead of re-pulling is what keeps a server that hands the cursor back
		// unchanged from becoming a hot loop (#181); the batch is already applied, idempotently.
		if response.next_position <= after {
			error!(
				cursor = after,
				next_position = response.next_position,
				events = response.events.len(),
				"scope events: server returned events but did not advance the cursor — applied this batch and ending the drain; scope changes stop arriving until this is fixed (needs intervention)"
			);
			return Ok(Pull { wedged: true, ..Pull::default() });
		}
		self.advance_cursor(after, response.next_position).await?;
		let newest = response.events.iter().map(|e| e.occurred_at).max().unwrap_or_default();
		info!(
			cursor = response.next_position,
			events = response.events.len(),
			applied,
			parked,
			parked_total = self.parked_total().await?,
			lag_secs = unix_now().saturating_sub(newest),
			"scope events: batch consumed"
		);
		Ok(Pull {
			progressed: true,
			more: response.events.len() >= PULL_LIMIT as usize,
			wedged: false,
		})
	}

	/// Re-pull every parked position alone and apply it if this build can read it now. A
	/// position that no longer resolves to the event parked there is left for a human:
	/// concierge never deletes stream rows, so that is a rewound or replaced stream.
	async fn replay_parked(&self, client: &mut ScopeEventsClient<Channel>) -> color_eyre::Result<()> {
		let parked: Vec<(i64, String)> = sqlx::query_as("SELECT position, event_id FROM scope_events_parked ORDER BY position")
			.fetch_all(&self.pool)
			.await?;
		if parked.is_empty() {
			return Ok(());
		}
		info!(count = parked.len(), "scope events: replaying parked events");
		for (position, event_id) in parked {
			let page = client.pull_scope_events(self.request(position - 1, 1)?).await?.into_inner();
			let resolved = page.events.into_iter().next().filter(|e| e.position == position && e.event_id == event_id);
			let Some(event) = resolved else {
				error!(position, %event_id, "scope events: a parked position no longer resolves to the event parked there (needs intervention)");
				self.touch_parked(position).await?;
				continue;
			};
			if self.apply(&event).await? == Outcome::Parked {
				self.touch_parked(position).await?;
				continue;
			}
			sqlx::query("DELETE FROM scope_events_parked WHERE position = $1").bind(position).execute(&self.pool).await?;
			info!(position, %event_id, "scope events: a parked event was applied by this build");
		}
		Ok(())
	}

	async fn touch_parked(&self, position: i64) -> Result<(), sqlx::Error> {
		sqlx::query("UPDATE scope_events_parked SET last_replay_at = now() WHERE position = $1")
			.bind(position)
			.execute(&self.pool)
			.await?;
		Ok(())
	}

	async fn parked_total(&self) -> Result<i64, sqlx::Error> {
		sqlx::query_scalar("SELECT count(*) FROM scope_events_parked").fetch_one(&self.pool).await
	}

	async fn cursor(&self) -> Result<i64, sqlx::Error> {
		sqlx::query_scalar("SELECT position FROM scope_events_cursor WHERE id = TRUE").fetch_one(&self.pool).await
	}

	/// Monotonic and compare-and-set, as `bridge_cursor`'s advance.
	async fn advance_cursor(&self, from: i64, to: i64) -> Result<(), sqlx::Error> {
		sqlx::query("UPDATE scope_events_cursor SET position = $2, updated_at = now() WHERE id = TRUE AND position = $1")
			.bind(from)
			.bind(to)
			.execute(&self.pool)
			.await?;
		Ok(())
	}

	async fn apply(&self, event: &ScopeEvent) -> Result<Outcome, sqlx::Error> {
		match Kind::try_from(event.kind) {
			Ok(Kind::ScopeChanged) => match &event.scope_snapshot {
				Some(snapshot) => self.apply_scope(event, snapshot).await,
				None => self.park_alone(event, "SCOPE_CHANGED without its scope snapshot").await,
			},
			Ok(Kind::GrantsChanged | Kind::MemberJoined) => Ok(Outcome::Ignored),
			Ok(Kind::Unspecified) | Err(_) => self.park_alone(event, "a scope event kind this build cannot name").await,
		}
	}

	async fn park_alone(&self, event: &ScopeEvent, reason: &'static str) -> Result<Outcome, sqlx::Error> {
		let mut tx = self.pool.begin().await?;
		park(&mut tx, event, reason).await?;
		tx.commit().await?;
		Ok(Outcome::Parked)
	}

	async fn apply_scope(&self, event: &ScopeEvent, snapshot: &Scope) -> Result<Outcome, sqlx::Error> {
		if snapshot.id == "main" {
			// The root is no row here (0052's CHECK); concierge does not announce it either.
			warn!(event_id = %event.event_id, "scope events: SCOPE_CHANGED for `main` ignored");
			return Ok(Outcome::Ignored);
		}
		if snapshot.id != event.scope {
			return self.park_alone(event, "SCOPE_CHANGED whose snapshot names another scope").await;
		}
		let Ok(id) = ScopeId::parse(&snapshot.id) else {
			return self.park_alone(event, "a scope id outside banking's format").await;
		};
		let Ok(version) = i64::try_from(snapshot.version) else {
			return self.park_alone(event, "a scope version past BIGINT").await;
		};
		let state = match WireScopeState::try_from(snapshot.state) {
			Ok(WireScopeState::Active) => Some(ScopeState::Active),
			Ok(WireScopeState::Frozen) => Some(ScopeState::Frozen),
			Ok(WireScopeState::Closed) => Some(ScopeState::Closed),
			Ok(WireScopeState::Unspecified) | Err(_) => None,
		};
		let display_name = match snapshot.display_name.chars().count() {
			1..=MAX_DISPLAY_NAME_CHARS => snapshot.display_name.as_str(),
			_ => id.as_str(),
		};

		let mut tx = self.pool.begin().await?;
		// The row lock is what a move in takes FOR SHARE while it decides: a state written
		// here waits for a move already deciding, and every move after it reads the new one.
		let stored: Option<(String, i64)> = sqlx::query_as("SELECT state, version FROM scopes WHERE id = $1 FOR UPDATE")
			.bind(id.as_str())
			.fetch_optional(&mut *tx)
			.await?;
		let outcome = match (stored, state) {
			(None, state) => {
				// Unknown state: registered taking nothing in, at version 0 so the replay applies.
				let (written, version) = state.map_or((ScopeState::Frozen, 0), |state| (state, version));
				register(&mut tx, &id, written, display_name, version).await?;
				info!(scope = %id, state = written.as_str(), version, "scope events: scope registered from concierge");
				if state.is_none() {
					park(&mut tx, event, "a scope state this build cannot name").await?;
					Outcome::Parked
				} else {
					Outcome::Applied
				}
			}
			(Some((_, stored_version)), _) if version <= stored_version => Outcome::Ignored,
			(Some(_), None) => {
				sqlx::query("UPDATE scopes SET state = 'frozen', updated_at = now() WHERE id = $1 AND state = 'active'")
					.bind(id.as_str())
					.execute(&mut *tx)
					.await?;
				error!(scope = %id, wire_state = snapshot.state, version, "scope events: a scope state this build cannot name — the scope takes no money in until a build that knows it replays the event");
				park(&mut tx, event, "a scope state this build cannot name").await?;
				Outcome::Parked
			}
			(Some((stored_state, _)), Some(state)) => {
				let stored_state = ScopeState::parse(&stored_state).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
				// CLOSED is terminal on concierge. A newer snapshot reopening one is a fault
				// there, and the money plane does not follow it back into taking money in.
				let written = if stored_state == ScopeState::Closed && state != ScopeState::Closed {
					error!(scope = %id, asked = state.as_str(), version, "scope events: concierge reopened a CLOSED scope — kept closed (needs intervention)");
					ScopeState::Closed
				} else {
					state
				};
				sqlx::query("UPDATE scopes SET state = $2, display_name = $3, version = $4, updated_at = now() WHERE id = $1")
					.bind(id.as_str())
					.bind(written.as_str())
					.bind(display_name)
					.bind(version)
					.execute(&mut *tx)
					.await?;
				if written != stored_state {
					warn!(scope = %id, from = stored_state.as_str(), to = written.as_str(), version, "scope events: scope state changed");
				}
				Outcome::Applied
			}
		};
		tx.commit().await?;
		Ok(outcome)
	}
}

/// A scope banking has not seen: its row, its policy at the column defaults (0052's
/// "a scope announced before its policy is set"), and the fee collector banking opens in
/// every scope, as 0052 did for the seeded two.
async fn register(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: &ScopeId, state: ScopeState, display_name: &str, version: i64) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO scopes (id, state, display_name, version) VALUES ($1, $2, $3, $4)")
		.bind(id.as_str())
		.bind(state.as_str())
		.bind(display_name)
		.bind(version)
		.execute(&mut **tx)
		.await?;
	sqlx::query("INSERT INTO scope_policies (scope) VALUES ($1) ON CONFLICT (scope) DO NOTHING")
		.bind(id.as_str())
		.execute(&mut **tx)
		.await?;
	sqlx::query("INSERT INTO scope_accounts (scope, label, owner_client) VALUES ($1, 'fee', 'banking') ON CONFLICT (scope, label) DO NOTHING")
		.bind(id.as_str())
		.execute(&mut **tx)
		.await?;
	Ok(())
}

/// `ON CONFLICT DO NOTHING` keeps a re-park from resetting `parked_at`, the one measure of
/// how long an event has waited.
async fn park(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, event: &ScopeEvent, reason: &'static str) -> Result<(), sqlx::Error> {
	sqlx::query("INSERT INTO scope_events_parked (position, event_id, scope, kind, reason) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (position) DO NOTHING")
		.bind(event.position)
		.bind(&event.event_id)
		.bind(&event.scope)
		.bind(event.kind)
		.bind(reason)
		.execute(&mut **tx)
		.await?;
	warn!(position = event.position, event_id = %event.event_id, scope = %event.scope, kind = event.kind, reason, "scope events: parked for a build that can read it");
	Ok(())
}

fn unix_now() -> i64 {
	// A clock before the epoch reads as lag 0 rather than failing the cycle over a log field.
	SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
