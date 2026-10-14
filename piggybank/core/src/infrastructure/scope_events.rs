//! The scope registry's projection of concierge — the consumer of `ScopeEvents.PullScopeEvents`.
//!
//! `scopes` (migration 0052) is what admission reads when a user moves money into a scope's
//! compartment; until this consumer it held only what 0052 seeded, so a scope concierge
//! froze, closed or registered never reached the money plane. One background task pulls the
//! stream with the shared bridge token — the unfiltered reader, every scope — and applies
//! each SCOPE_CHANGED snapshot to `scopes`. A scope it registers is closed to moves in until
//! an operator sets its policy ([`CLOSED_UNTIL_SET_KYC`]). GRANTS_CHANGED and MEMBER_JOINED
//! are consumed and dropped: a service's rights here are banking's own configuration, never
//! concierge's grants, and users arrive over the lifecycle bridge.
//!
//! At-least-once delivery is absorbed by the scope's `version`: a snapshot applies only when
//! it is newer than the row, so a redelivery is a no-op and an older snapshot can never undo
//! a newer FROZEN. The cursor (`scope_events_cursor`) advances only after the whole batch was
//! applied or parked.
//!
//! ONE CONSUMER, WHATEVER THE REPLICA COUNT. The task follows the stream only while it holds
//! the session advisory lock [`singleton::keys::SCOPE_EVENTS`]; another replica blocks on it
//! and takes over when the holder's session ends. Two consumers would not corrupt a row —
//! each write is versioned — but they would race the cursor and register the same scope
//! twice, so the compare-and-set cursor and the `ON CONFLICT` registration only report what
//! the lock already rules out.
//!
//! THE STREAM IS TRUSTED ONLY AS FAR AS IT CHECKS OUT. concierge never deletes stream rows,
//! so the event the cursor stands on (`last_event_id`) must still be at that position; at
//! start and every [`CONTINUITY_CHECK_INTERVAL`] it is re-pulled alone, and a position that
//! answers another event or nothing means the stream was rewound or replaced — the consumer
//! stops applying and says so, because following a restored stream would replay old
//! snapshots against versions it can no longer be compared with. Every page is checked
//! against the bridge token's contract before anything in it is applied ([`page_fault`]).
//! Neither condition moves the cursor; both need a human.
//!
//! A STALL IS ALERTED, NOT JUST LOGGED. `last_pulled_at` records the last well-formed page,
//! empty included; reconciliation reads it through [`freshness`] and alerts when it falls
//! behind — unless concierge's last answer was UNIMPLEMENTED (a concierge that predates
//! ScopeEvents), which is the expected state until one that serves it is deployed.
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

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use domain::scopes::{ScopeId, ScopeState};
use evconcierge_contracts::concierge::v1::{
	PullScopeEventsRequest, PullScopeEventsResponse, Scope, ScopeEvent, ScopeState as WireScopeState, scope_event::Kind, scope_events_client::ScopeEventsClient,
};
use sqlx::{PgPool, Postgres, pool::PoolConnection};
use tokio_util::sync::CancellationToken;
use tonic::{Request, metadata::MetadataValue, transport::Channel};
use tracing::{error, info, warn};

use super::singleton;

/// Rows requested per pull; concierge caps it at its own ceiling.
const PULL_LIMIT: u32 = 256;

/// Ceiling on the retry delay after consecutive stalled cycles — the lifecycle bridge's,
/// for the same reasons (`bridge::BACKOFF_MAX`).
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How often the event under the cursor is re-checked against concierge. A rewind is an
/// operator's restore, not something that happens between two polls, and each check is one
/// extra pull; minutes bound how long a rewound stream goes unnoticed.
pub const CONTINUITY_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Ceiling on the retry delay when taking the consumer lock fails outright (the pool, not a
/// holder: a holder makes the acquire block, not fail).
const LOCK_RETRY_MAX: Duration = Duration::from_secs(30);

/// `scopes.display_name`'s CHECK. concierge caps it at 80 bytes, so only an empty name
/// (concierge's default) or a contract change trips it; the id stands in rather than the
/// event being refused.
const MAX_DISPLAY_NAME_CHARS: usize = 128;

/// `min_kyc_level` no KYC tier reaches (the column is INTEGER): what a scope announced by
/// concierge gets until an operator sets its policy (docs/RUNBOOK-scope-policy.md). A scope's
/// rules are banking's to decide, and the column defaults would admit every user with no
/// limit — so a new scope is closed to moves in until someone has decided them. Moves out to
/// `main` never read the policy and stay open.
pub const CLOSED_UNTIL_SET_KYC: i32 = i32::MAX;

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
	/// Nothing could be applied and the cursor cannot move until someone intervenes — a
	/// malformed page or a rewound stream. Backs the cycle off like a failure.
	wedged: bool,
}

/// The last verdict on the event under the cursor, and when it was reached.
#[derive(Clone, Copy, Debug)]
enum Continuity {
	Unchecked,
	Intact(Instant),
	Broken(Instant),
}

impl Continuity {
	fn due(self) -> bool {
		match self {
			Self::Unchecked => true,
			Self::Intact(at) | Self::Broken(at) => at.elapsed() >= CONTINUITY_CHECK_INTERVAL,
		}
	}
}

/// Why [`ScopeEventsConsumer::follow`] returned.
enum Stop {
	Shutdown,
	LostLock,
}

/// How current the registry is against concierge's scope stream; see [`freshness`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamFreshness {
	/// A well-formed page arrived within the threshold.
	Fresh { lag_secs: i64 },
	/// Behind, and concierge's latest answer since then was UNIMPLEMENTED: it predates
	/// ScopeEvents. Expected until a concierge that serves the stream is deployed.
	Unserved { lag_secs: i64 },
	/// Behind for any other reason — concierge unreachable, a rewound stream, a malformed
	/// page, or no replica holding the consumer lock.
	Stalled { lag_secs: i64 },
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

	/// Run until `shutdown` is cancelled, following the stream only while holding the
	/// consumer lock. A lost lock session is re-acquired rather than exited on.
	pub async fn run(self, shutdown: CancellationToken) {
		let mut lock_backoff = Duration::from_millis(500);
		loop {
			let mut lock = tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				lock = singleton::acquire(&self.pool, singleton::keys::SCOPE_EVENTS) => match lock {
					Ok(lock) => lock,
					Err(err) => {
						error!(retry_in = ?lock_backoff, "scope events: could not take the consumer lock: {err}");
						tokio::select! {
							() = shutdown.cancelled() => return,
							() = tokio::time::sleep(lock_backoff) => {},
						}
						lock_backoff = lock_backoff.saturating_mul(2).min(LOCK_RETRY_MAX);
						continue;
					}
				},
			};
			lock_backoff = Duration::from_millis(500);
			info!(every = ?self.poll_interval, "scope events: holding the consumer lock — following concierge's scope stream");
			let stop = self.follow(&mut lock, &shutdown).await;
			// Closed, not dropped: a dropped connection returns to the pool still holding the
			// lock, and the next acquire (ours or a test's next consumer) would wait on it.
			if let Err(err) = lock.close().await {
				warn!("scope events: closing the consumer lock's connection failed: {err}");
			}
			match stop {
				Stop::Shutdown => {
					info!("scope events: shutdown requested — stopping");
					return;
				}
				Stop::LostLock => error!("scope events: lost the consumer lock's session — re-acquiring"),
			}
		}
	}

	/// The consumer loop under the lock. A cycle that had work and got none of it done backs
	/// off exponentially up to [`BACKOFF_MAX`]; an idle or progressing cycle waits the poll
	/// interval — the lifecycle bridge's rule, argued there (`BridgeConsumer::run`).
	///
	/// The parked rows are replayed once per lock tenure, before the first pull: what parks
	/// them is this build not knowing something, so only a new build can resolve one. A
	/// position whose re-pull fails (concierge down at boot) keeps the replay pending, and it
	/// is retried every cycle — beside the drain, never instead of it: a parked row must not
	/// hold back the freezes behind it, which is why it was parked.
	async fn follow(&self, lock: &mut PoolConnection<Postgres>, shutdown: &CancellationToken) -> Stop {
		let mut client = ScopeEventsClient::new(self.channel.clone());
		let ceiling = BACKOFF_MAX.max(self.poll_interval);
		let mut backoff: Option<Duration> = None;
		let mut replayed = false;
		let mut continuity = Continuity::Unchecked;
		loop {
			let mut cycle = Pull::default();
			let mut failure = None;
			if !replayed {
				match self.replay_parked(&mut client).await {
					Ok(0) => replayed = true,
					Ok(unresolved) => warn!(unresolved, "scope events: some parked events could not be re-pulled — retrying next cycle"),
					Err(err) => warn!("scope events: could not read the parked events — retrying next cycle: {err}"),
				}
			}
			if continuity.due() {
				match self.check_continuity(&mut client).await {
					Ok(true) => continuity = Continuity::Intact(Instant::now()),
					Ok(false) => continuity = Continuity::Broken(Instant::now()),
					Err(err) => failure = Some(err),
				}
			}
			match continuity {
				// A failed re-check skips this cycle's drain; the next cycle checks again.
				Continuity::Intact(_) if failure.is_none() => failure = self.drain(&mut client, &mut cycle).await.err(),
				Continuity::Broken(_) => cycle.wedged = true,
				Continuity::Intact(_) | Continuity::Unchecked => {}
			}
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
				let code = err.downcast_ref::<tonic::Status>().map(tonic::Status::code);
				if code == Some(tonic::Code::Unimplemented)
					&& let Err(err) = self.note_unimplemented().await
				{
					warn!("scope events: could not record concierge's UNIMPLEMENTED: {err}");
				}
				let hint = match code {
					Some(tonic::Code::Unavailable) => " (is concierge running?)",
					// A concierge older than the scope stream answers every pull this way; the
					// registry keeps its last state until one that serves it is deployed.
					Some(tonic::Code::Unimplemented) => " (concierge predates ScopeEvents)",
					_ => "",
				};
				warn!(retry_in = ?wait, progressed = cycle.progressed, "scope events: pull/apply cycle failed, retrying{hint}: {err}");
			}
			tokio::select! {
				() = shutdown.cancelled() => return Stop::Shutdown,
				() = tokio::time::sleep(wait) => {},
			}
			// A reaped backend releases the lock silently; following on would make two consumers.
			if !singleton::still_held(lock).await {
				return Stop::LostLock;
			}
		}
	}

	/// Pull and apply until a short batch says the stream is drained. `cycle` survives the
	/// `?` so [`Self::follow`] knows what was committed before an error cut the drain short.
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

	/// Whether the event the cursor stands on is still at its position on concierge.
	/// `false` has already been reported; the caller stops applying.
	async fn check_continuity(&self, client: &mut ScopeEventsClient<Channel>) -> color_eyre::Result<bool> {
		let (position, last_event_id): (i64, Option<String>) = sqlx::query_as("SELECT position, last_event_id FROM scope_events_cursor WHERE id = TRUE")
			.fetch_one(&self.pool)
			.await?;
		let expected = match (position, last_event_id) {
			(0, _) => return Ok(true),
			(_, Some(expected)) => expected,
			// Every advance writes the id with the position, so only a hand-edited cursor
			// lands here; the next batch restores the check.
			(_, None) => {
				warn!(
					cursor = position,
					"scope events: the cursor carries no event id — its continuity cannot be checked until the next batch"
				);
				return Ok(true);
			}
		};
		let page = client.pull_scope_events(self.request(position - 1, 1)?).await?.into_inner();
		match page.events.first() {
			Some(event) if event.position == position && event.event_id == expected => Ok(true),
			found => {
				error!(
					cursor = position,
					expected_event_id = %expected,
					found_position = found.map(|e| e.position),
					found_event_id = found.map(|e| e.event_id.as_str()),
					"scope events: concierge's scope stream no longer holds the event the cursor stands on — rewound or replaced; nothing more is applied and the cursor is held (needs intervention)"
				);
				Ok(false)
			}
		}
	}

	async fn pull_and_apply(&self, client: &mut ScopeEventsClient<Channel>) -> color_eyre::Result<Pull> {
		let after = self.cursor().await?;
		let response = client.pull_scope_events(self.request(after, PULL_LIMIT)?).await?.into_inner();
		if let Some(fault) = page_fault(after, &response) {
			error!(
				cursor = after,
				next_position = response.next_position,
				events = response.events.len(),
				fault,
				"scope events: concierge answered a page that breaks the stream's contract — nothing in it applied, the cursor held; scope changes stop arriving until this is fixed (needs intervention)"
			);
			return Ok(Pull { wedged: true, ..Pull::default() });
		}
		let Some(last) = response.events.last() else {
			self.note_pulled().await?;
			return Ok(Pull::default());
		};
		let (mut applied, mut parked) = (0usize, 0usize);
		for event in &response.events {
			match self.apply(event).await? {
				Outcome::Applied => applied += 1,
				Outcome::Parked => parked += 1,
				Outcome::Ignored => {}
			}
		}
		self.advance_cursor(after, response.next_position, &last.event_id).await?;
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

	/// Re-pull every parked position alone and apply it if this build can read it now.
	/// Returns how many positions could not be re-pulled or written — those stay parked and
	/// the caller retries the replay. A position that no longer resolves to the event parked
	/// there is left for a human: concierge never deletes stream rows, so that is a rewound
	/// or replaced stream.
	async fn replay_parked(&self, client: &mut ScopeEventsClient<Channel>) -> Result<usize, sqlx::Error> {
		let parked: Vec<(i64, String)> = sqlx::query_as("SELECT position, event_id FROM scope_events_parked ORDER BY position")
			.fetch_all(&self.pool)
			.await?;
		if parked.is_empty() {
			return Ok(0);
		}
		info!(count = parked.len(), "scope events: replaying parked events");
		let mut unresolved = 0;
		for (position, event_id) in parked {
			if let Err(err) = self.replay_one(client, position, &event_id).await {
				unresolved += 1;
				warn!(position, %event_id, "scope events: replaying a parked event failed — it stays parked: {err}");
				if let Err(err) = self.touch_parked(position).await {
					warn!(position, "scope events: could not stamp the parked event's replay attempt: {err}");
				}
			}
		}
		Ok(unresolved)
	}

	async fn replay_one(&self, client: &mut ScopeEventsClient<Channel>, position: i64, event_id: &str) -> color_eyre::Result<()> {
		let before = position
			.checked_sub(1)
			.ok_or_else(|| color_eyre::eyre::eyre!("parked position {position} has no position before it to pull after"))?;
		let page = client.pull_scope_events(self.request(before, 1)?).await?.into_inner();
		let resolved = page.events.into_iter().next().filter(|e| e.position == position && e.event_id == event_id);
		let Some(event) = resolved else {
			error!(position, %event_id, "scope events: a parked position no longer resolves to the event parked there (needs intervention)");
			self.touch_parked(position).await?;
			return Ok(());
		};
		if self.apply(&event).await? == Outcome::Parked {
			self.touch_parked(position).await?;
			return Ok(());
		}
		sqlx::query("DELETE FROM scope_events_parked WHERE position = $1").bind(position).execute(&self.pool).await?;
		info!(position, %event_id, "scope events: a parked event was applied by this build");
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

	async fn note_pulled(&self) -> Result<(), sqlx::Error> {
		sqlx::query("UPDATE scope_events_cursor SET last_pulled_at = now() WHERE id = TRUE").execute(&self.pool).await?;
		Ok(())
	}

	async fn note_unimplemented(&self) -> Result<(), sqlx::Error> {
		sqlx::query("UPDATE scope_events_cursor SET last_unimplemented_at = now() WHERE id = TRUE")
			.execute(&self.pool)
			.await?;
		Ok(())
	}

	/// Compare-and-set, as `bridge_cursor`'s advance: it moves only from where the batch was
	/// pulled. Under the consumer lock a miss means a second writer the lock should have
	/// excluded, so it is reported rather than retried — the batch itself was idempotent.
	async fn advance_cursor(&self, from: i64, to: i64, event_id: &str) -> Result<(), sqlx::Error> {
		let moved = sqlx::query("UPDATE scope_events_cursor SET position = $2, last_event_id = $3, last_pulled_at = now(), updated_at = now() WHERE id = TRUE AND position = $1")
			.bind(from)
			.bind(to)
			.bind(event_id)
			.execute(&self.pool)
			.await?
			.rows_affected();
		if moved == 0 {
			warn!(
				from,
				to, "scope events: the cursor was no longer where this batch was pulled from — another consumer moved it; the batch was applied idempotently and the cursor left as found"
			);
		}
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
				if !register(&mut tx, &id, written, display_name, version).await? {
					warn!(
						scope = %id,
						version,
						position = event.position,
						"scope events: the scope appeared while being registered — a second consumer is writing the registry; this snapshot was not applied"
					);
					Outcome::Ignored
				} else if state.is_none() {
					info!(scope = %id, state = written.as_str(), version, "scope events: scope registered from concierge");
					park(&mut tx, event, "a scope state this build cannot name").await?;
					Outcome::Parked
				} else {
					info!(scope = %id, state = written.as_str(), version, "scope events: scope registered from concierge");
					Outcome::Applied
				}
			}
			// A redelivery: at-least-once is the stream's contract, nothing to say.
			(Some((_, stored_version)), _) if version == stored_version => Outcome::Ignored,
			// An older snapshot after a newer one is not something concierge's ordering produces
			// on its own: a rewound cursor or a stream replayed from 0 is how it gets here.
			(Some((_, stored_version)), _) if version < stored_version => {
				warn!(
					scope = %id,
					stored_version,
					version,
					position = event.position,
					"scope events: a snapshot older than the registry's ignored"
				);
				Outcome::Ignored
			}
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

/// What makes a page unusable, checked before anything in it is applied. The bridge token
/// reads unfiltered, so every row past the cursor is shown: positions rise strictly from
/// the cursor, a page ends on its last event, and an empty page ends where it began. A page
/// that scanned past rows it did not show is unreachable for this token and is treated as a
/// fault, not followed.
fn page_fault(after: i64, page: &PullScopeEventsResponse) -> Option<&'static str> {
	let mut previous = after;
	for event in &page.events {
		if event.position <= previous {
			return Some(if previous == after {
				"an event at or before the cursor"
			} else {
				"positions that do not strictly increase"
			});
		}
		previous = event.position;
	}
	(page.next_position != previous).then_some(if page.events.is_empty() {
		"an empty page that moves the cursor"
	} else {
		"a next_position other than the last event's"
	})
}

/// Where the registry stands against the stream, for reconciliation's alert. Lag runs from
/// the last well-formed page; a consumer that never got one is measured from when the
/// cursor row was last written — the migration, on a fresh deploy. An UNIMPLEMENTED newer
/// than that page and inside the threshold explains the lag.
pub async fn freshness(pool: &PgPool, stall_after: Duration) -> Result<StreamFreshness, sqlx::Error> {
	let threshold = i64::try_from(stall_after.as_secs()).unwrap_or(i64::MAX);
	let (lag_secs, unserved): (i64, bool) = sqlx::query_as(
		"SELECT EXTRACT(EPOCH FROM now() - COALESCE(last_pulled_at, updated_at))::BIGINT, \
		 COALESCE(last_unimplemented_at > COALESCE(last_pulled_at, '-infinity'::timestamptz) \
		 AND last_unimplemented_at > now() - make_interval(secs => $1::float8), FALSE) \
		 FROM scope_events_cursor WHERE id = TRUE",
	)
	.bind(threshold)
	.fetch_one(pool)
	.await?;
	Ok(match (lag_secs > threshold, unserved) {
		(false, _) => StreamFreshness::Fresh { lag_secs },
		(true, true) => StreamFreshness::Unserved { lag_secs },
		(true, false) => StreamFreshness::Stalled { lag_secs },
	})
}

/// A scope banking has not seen: its row, a policy closed to moves in
/// ([`CLOSED_UNTIL_SET_KYC`]), and the fee collector banking opens in every scope, as 0052
/// did for the seeded two. `false` when the row was already there.
async fn register(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: &ScopeId, state: ScopeState, display_name: &str, version: i64) -> Result<bool, sqlx::Error> {
	let inserted = sqlx::query("INSERT INTO scopes (id, state, display_name, version) VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING")
		.bind(id.as_str())
		.bind(state.as_str())
		.bind(display_name)
		.bind(version)
		.execute(&mut **tx)
		.await?
		.rows_affected();
	sqlx::query("INSERT INTO scope_policies (scope, min_kyc_level) VALUES ($1, $2) ON CONFLICT (scope) DO NOTHING")
		.bind(id.as_str())
		.bind(CLOSED_UNTIL_SET_KYC)
		.execute(&mut **tx)
		.await?;
	sqlx::query("INSERT INTO scope_accounts (scope, label, owner_client) VALUES ($1, 'fee', 'banking') ON CONFLICT (scope, label) DO NOTHING")
		.bind(id.as_str())
		.execute(&mut **tx)
		.await?;
	Ok(inserted == 1)
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

#[cfg(test)]
mod tests {
	use super::*;

	fn page(positions: &[i64], next_position: i64) -> PullScopeEventsResponse {
		PullScopeEventsResponse {
			events: positions.iter().map(|&position| ScopeEvent { position, ..ScopeEvent::default() }).collect(),
			next_position,
		}
	}

	#[test]
	fn a_page_that_keeps_the_bridge_contract_passes() {
		assert_eq!(page_fault(10, &page(&[11, 14, 15], 15)), None);
		assert_eq!(page_fault(10, &page(&[], 10)), None, "an empty page that stays put");
	}

	#[test]
	fn a_page_that_breaks_the_bridge_contract_is_named() {
		assert_eq!(page_fault(10, &page(&[10, 11], 11)), Some("an event at or before the cursor"));
		assert_eq!(page_fault(10, &page(&[9], 9)), Some("an event at or before the cursor"));
		assert_eq!(page_fault(10, &page(&[11, 13, 12], 12)), Some("positions that do not strictly increase"));
		assert_eq!(page_fault(10, &page(&[11, 11], 11)), Some("positions that do not strictly increase"));
		assert_eq!(page_fault(10, &page(&[11, 12], 14)), Some("a next_position other than the last event's"));
		assert_eq!(page_fault(10, &page(&[11, 12], 11)), Some("a next_position other than the last event's"));
		assert_eq!(page_fault(10, &page(&[], 12)), Some("an empty page that moves the cursor"));
		assert_eq!(page_fault(10, &page(&[], 3)), Some("an empty page that moves the cursor"));
	}
}
