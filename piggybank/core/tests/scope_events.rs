//! The scope registry following concierge's scope stream — real Postgres (and TigerBeetle
//! where money moves), a fake concierge `ScopeEvents` served in-process over localhost.
//!
//! Every test registers scopes under fresh ids, so the registry rows never collide. The
//! cursor and the parked table are single and shared, so the tests take [`serial`] for
//! their whole life and start each feed from position 0; a test that also moves money takes
//! the outbox guard after it, always in that order.
//!
//! Concurrency is structured: the fake server, the consumer and the asserting driver are
//! branches of one `join!`, wound down by a token the driver cancels.

use std::{
	future::Future,
	net::SocketAddr,
	sync::{Arc, LazyLock, Mutex},
	time::Duration,
};

use domain::{
	auth::AuthSubject,
	balance::LedgerAccountKey,
	money::{Network, TxRef, Usdt},
	scopes::{Compartment, MoveRefusal, ScopeId},
	users::{Email, UserId},
};
use evconcierge_contracts::concierge::v1::{
	PullScopeEventsRequest, PullScopeEventsResponse, Scope, ScopeEvent, ScopeGrants, ScopeState as WireScopeState,
	scope_event::Kind,
	scope_events_server::{ScopeEvents, ScopeEventsServer},
};
use piggybank_core::{
	application::{
		balance as balance_app,
		compartments::{self as compartments_app, CompartmentPorts, MoveError},
	},
	infrastructure::{
		compartments::PgCompartments,
		deposits::PgDeposits,
		outflow::PgOutflowPolicy,
		relay::Relay,
		scope_events::{CLOSED_UNTIL_SET_KYC, ScopeEventsConsumer, StreamFreshness, freshness},
		singleton,
		users::PgUsers,
	},
	ports::{UserRepository, compartments::RecordedMove},
};
use sqlx::PgPool;
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, transport::Server};
use uuid::Uuid;

mod common;

const BRIDGE_TOKEN: &str = "test-bridge-token";

static SERIAL: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Held for a whole test: one cursor, one parked table.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
	SERIAL.lock().await
}

/// What the fake concierge serves; a test pushes to it while the consumer runs.
type Feed = Arc<Mutex<Vec<ScopeEvent>>>;

/// Every `(after_position, limit)` the fake was asked for, in order.
type Served = Arc<Mutex<Vec<(i64, u32)>>>;

/// How the fake answers.
#[derive(Clone, Default)]
enum Mode {
	/// The feed, as concierge serves the bridge token.
	#[default]
	Serve,
	/// A concierge that predates ScopeEvents.
	Unimplemented,
	/// The feed, except that every single-event pull (a parked replay, a continuity check)
	/// fails.
	FailSingle,
	/// The same page for every pull, whatever it asks for.
	Canned(PullScopeEventsResponse),
}

/// Serves the feed by each event's own `position`, `> after_position`, capped by `limit`;
/// `next_position` is the last position returned — the unfiltered (bridge-token) reader's
/// contract. Refuses any other credential, as concierge does.
#[derive(Clone, Default)]
struct FakeScopeEvents {
	feed: Feed,
	served: Served,
	mode: Mode,
}

#[tonic::async_trait]
impl ScopeEvents for FakeScopeEvents {
	async fn pull_scope_events(&self, request: Request<PullScopeEventsRequest>) -> Result<Response<PullScopeEventsResponse>, Status> {
		match request.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
			Some(value) if value == format!("Bearer {BRIDGE_TOKEN}") => {}
			_ => return Err(Status::unauthenticated("bad bridge token")),
		}
		let req = request.into_inner();
		self.served.lock().unwrap().push((req.after_position, req.limit));
		match &self.mode {
			Mode::Serve => {}
			Mode::Unimplemented => return Err(Status::unimplemented("ScopeEvents")),
			Mode::FailSingle if req.limit == 1 => return Err(Status::internal("single pulls fail")),
			Mode::FailSingle => {}
			Mode::Canned(page) => return Ok(Response::new(page.clone())),
		}
		let feed = self.feed.lock().unwrap().clone();
		let events: Vec<ScopeEvent> = feed.into_iter().filter(|e| e.position > req.after_position).take(req.limit.max(1) as usize).collect();
		let next_position = events.last().map_or(req.after_position, |e| e.position);
		Ok(Response::new(PullScopeEventsResponse { events, next_position }))
	}
}

fn fresh_scope() -> String {
	format!("t_{}", &Uuid::new_v4().simple().to_string()[..12])
}

fn changed(position: i64, scope: &str, state: WireScopeState, version: u64) -> ScopeEvent {
	ScopeEvent {
		event_id: Uuid::new_v4().to_string(),
		position,
		scope: scope.to_owned(),
		kind: Kind::ScopeChanged as i32,
		occurred_at: 1_790_000_000,
		sequence: version,
		scope_snapshot: Some(Scope {
			id: scope.to_owned(),
			namespace: scope.to_owned(),
			parent: "main".into(),
			display_name: format!("Scope {scope}"),
			version,
			state: state as i32,
			..Scope::default()
		}),
		..ScopeEvent::default()
	}
}

/// A feed of `events`, the cursor at 0 and nothing parked.
async fn fresh_feed(pool: &PgPool, events: Vec<ScopeEvent>) -> Feed {
	sqlx::query("UPDATE scope_events_cursor SET position = 0, last_event_id = NULL, last_pulled_at = NULL, last_unimplemented_at = NULL WHERE id = TRUE")
		.execute(pool)
		.await
		.unwrap();
	sqlx::query("DELETE FROM scope_events_parked").execute(pool).await.unwrap();
	Arc::new(Mutex::new(events))
}

/// Run the fake server over `feed` and the consumer beside `driver`; return once the driver has.
async fn drive<Fut: Future<Output = ()>>(pool: &PgPool, feed: Feed, driver: Fut) {
	drive_fake(pool, FakeScopeEvents { feed, ..FakeScopeEvents::default() }, driver).await;
}

async fn drive_fake<Fut: Future<Output = ()>>(pool: &PgPool, fake: FakeScopeEvents, driver: Fut) {
	let addr: SocketAddr = {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind an ephemeral port");
		listener.local_addr().unwrap()
	};
	let stop = CancellationToken::new();
	let server = {
		let stop = stop.clone();
		async move {
			Server::builder()
				.add_service(ScopeEventsServer::new(fake))
				.serve_with_shutdown(addr, stop.cancelled_owned())
				.await
				.ok();
		}
	};
	let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect_lazy();
	let consumer = ScopeEventsConsumer::new(pool.clone(), channel, BRIDGE_TOKEN.to_owned(), Duration::from_millis(50)).run(stop.clone());
	let asserter = async {
		driver.await;
		stop.cancel();
	};
	tokio::join!(server, consumer, asserter);
}

/// Poll until `check` holds; the consumer works on its own interval.
async fn eventually<F, Fut>(mut check: F) -> bool
where
	F: FnMut() -> Fut,
	Fut: Future<Output = bool>, {
	for _ in 0..80 {
		if check().await {
			return true;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	false
}

async fn row(pool: &PgPool, scope: &str) -> Option<(String, i64)> {
	sqlx::query_as("SELECT state, version FROM scopes WHERE id = $1").bind(scope).fetch_optional(pool).await.unwrap()
}

async fn cursor(pool: &PgPool) -> i64 {
	sqlx::query_scalar("SELECT position FROM scope_events_cursor WHERE id = TRUE").fetch_one(pool).await.unwrap()
}

async fn parked(pool: &PgPool) -> Vec<(i64, String)> {
	sqlx::query_as("SELECT position, reason FROM scope_events_parked ORDER BY position")
		.fetch_all(pool)
		.await
		.unwrap()
}

async fn caught_up(pool: &PgPool, position: i64) -> bool {
	eventually(|| async { cursor(pool).await == position }).await
}

#[tokio::test]
async fn a_scope_banking_has_not_seen_is_registered_from_the_stream() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 3)]).await;
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 1).await, "the cursor never passed the event");
		assert_eq!(row(&pool, &scope).await, Some(("active".into(), 3)));
		let name: String = sqlx::query_scalar("SELECT display_name FROM scopes WHERE id = $1").bind(&scope).fetch_one(&pool).await.unwrap();
		assert_eq!(name, format!("Scope {scope}"));
		let policy: Option<i32> = sqlx::query_scalar("SELECT min_kyc_level FROM scope_policies WHERE scope = $1")
			.bind(&scope)
			.fetch_optional(&pool)
			.await
			.unwrap();
		assert_eq!(policy, Some(CLOSED_UNTIL_SET_KYC), "a registered scope is closed to moves in until its policy is set");
		let fee: Option<String> = sqlx::query_scalar("SELECT owner_client FROM scope_accounts WHERE scope = $1 AND label = 'fee'")
			.bind(&scope)
			.fetch_optional(&pool)
			.await
			.unwrap();
		assert_eq!(fee.as_deref(), Some("banking"), "and banking's fee collector in it");
	})
	.await;
}

#[tokio::test]
async fn a_redelivered_or_older_snapshot_changes_nothing() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	let frozen = changed(2, &scope, WireScopeState::Frozen, 2);
	let mut redelivered = frozen.clone();
	redelivered.position = 3;
	let events = vec![
		changed(1, &scope, WireScopeState::Active, 1),
		frozen,
		redelivered,
		// Older than what is stored: it must not thaw the scope.
		changed(4, &scope, WireScopeState::Active, 1),
		// A grant change is consumed and acted on by nobody here.
		ScopeEvent {
			event_id: Uuid::new_v4().to_string(),
			position: 5,
			scope: scope.clone(),
			kind: Kind::GrantsChanged as i32,
			auth_subject: "itest-subject".into(),
			grants: Some(ScopeGrants::default()),
			..ScopeEvent::default()
		},
	];
	let feed = fresh_feed(&pool, events).await;
	let mut stamped = None;
	drive(&pool, feed.clone(), async {
		assert!(caught_up(&pool, 5).await, "the cursor never passed the batch");
		assert_eq!(row(&pool, &scope).await, Some(("frozen".into(), 2)));
		assert!(parked(&pool).await.is_empty(), "nothing in the batch is unreadable");
		stamped = Some(
			sqlx::query_scalar::<_, String>("SELECT updated_at::text FROM scopes WHERE id = $1")
				.bind(&scope)
				.fetch_one(&pool)
				.await
				.unwrap(),
		);
	})
	.await;

	// The whole stream again, as after a rewound cursor: every snapshot is already reflected.
	sqlx::query("UPDATE scope_events_cursor SET position = 0 WHERE id = TRUE").execute(&pool).await.unwrap();
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 5).await, "the cursor never passed the replayed batch");
		assert_eq!(row(&pool, &scope).await, Some(("frozen".into(), 2)));
		let again: String = sqlx::query_scalar("SELECT updated_at::text FROM scopes WHERE id = $1")
			.bind(&scope)
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(Some(again), stamped, "a redelivery must not write the row");
	})
	.await;
}

#[tokio::test]
async fn an_unknown_kind_is_parked_the_cursor_moves_on_and_a_newer_build_applies_it() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let (behind, beside) = (fresh_scope(), fresh_scope());
	// A kind outside this build's enum, as concierge shipping ahead would send it.
	let unknown = ScopeEvent {
		kind: 99,
		scope_snapshot: None,
		..changed(1, &behind, WireScopeState::Active, 1)
	};
	let frozen = changed(2, &beside, WireScopeState::Frozen, 1);
	let feed = fresh_feed(&pool, vec![unknown.clone(), frozen.clone()]).await;
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 2).await, "an unreadable kind must not hold the cursor");
		assert_eq!(parked(&pool).await, vec![(1, "a scope event kind this build cannot name".to_owned())]);
		assert_eq!(row(&pool, &beside).await, Some(("frozen".into(), 1)), "the event behind it is applied");
		assert_eq!(row(&pool, &behind).await, None);
	})
	.await;

	// The upgrade: the same row at the same position, now a kind this build names.
	let readable = ScopeEvent {
		event_id: unknown.event_id,
		..changed(1, &behind, WireScopeState::Active, 1)
	};
	let feed = Arc::new(Mutex::new(vec![readable, frozen]));
	drive(&pool, feed, async {
		assert!(eventually(|| async { parked(&pool).await.is_empty() }).await, "the parked event was never replayed");
		assert_eq!(row(&pool, &behind).await, Some(("active".into(), 1)));
		assert_eq!(cursor(&pool).await, 2, "the replay does not move the cursor");
	})
	.await;
}

#[tokio::test]
async fn a_state_this_build_cannot_name_stops_money_in_until_a_build_that_can() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	let mut unnamed = changed(2, &scope, WireScopeState::Active, 2);
	unnamed.scope_snapshot.as_mut().unwrap().state = 7;
	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 1), unnamed.clone()]).await;
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 2).await, "the cursor never passed the batch");
		assert_eq!(row(&pool, &scope).await, Some(("frozen".into(), 1)), "fail closed, and the version left for the replay");
		assert_eq!(parked(&pool).await, vec![(2, "a scope state this build cannot name".to_owned())]);
	})
	.await;

	let mut understood = unnamed;
	understood.scope_snapshot.as_mut().unwrap().state = WireScopeState::Active as i32;
	let feed = Arc::new(Mutex::new(vec![changed(1, &scope, WireScopeState::Active, 1), understood]));
	drive(&pool, feed, async {
		assert!(eventually(|| async { parked(&pool).await.is_empty() }).await, "the parked event was never replayed");
		assert_eq!(row(&pool, &scope).await, Some(("active".into(), 2)));
	})
	.await;
}

#[tokio::test]
async fn a_closed_scope_is_not_reopened_by_a_later_snapshot() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Closed, 1), changed(2, &scope, WireScopeState::Active, 2)]).await;
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 2).await, "the cursor never passed the batch");
		assert_eq!(row(&pool, &scope).await, Some(("closed".into(), 2)));
	})
	.await;
}

/// A scope concierge freezes stops taking money in on the money plane, and the users'
/// money already in it still goes back to `main` (D24-7); CLOSED the same.
#[tokio::test]
async fn a_scope_frozen_on_concierge_refuses_money_in_and_lets_it_out() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let _outbox = common::outbox_serial().await;
	let Some(ledger) = common::seeded_ledger(&pool, "scope events").await else { return };
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(piggybank_core::infrastructure::custody::StubCustody), notify.clone());
	let (store, outflow) = (PgCompartments::new(pool.clone()), PgOutflowPolicy::new(pool.clone()));
	let ports = CompartmentPorts {
		store: &store,
		ledger: ledger.as_ref(),
		policy: &outflow,
		relay: &notify,
	};
	let usdt = |raw: &str| Usdt::parse_decimal(raw).unwrap();
	let key = || Uuid::new_v4().to_string();

	let users = PgUsers::new(pool.clone());
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user: UserId = users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(&pool, user, 1).await;
	let tx_ref = TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&PgDeposits::new(pool.clone()), &notify, tx_ref, user, Network::Bep20, usdt("20"))
		.await
		.unwrap();
	common::drain_to_quiescence(&relay, &pool).await;

	let scope = fresh_scope();
	let compartment = Compartment::parse(&scope).unwrap();
	let move_funds = |from: Compartment, to: Compartment, amount: &str, key: String| {
		let amount = usdt(amount);
		let ports = &ports;
		async move { compartments_app::move_funds(ports, user, from, to, amount, &key).await }
	};
	let refusal = |result: Result<RecordedMove, MoveError>| match result {
		Err(MoveError::Refused(refusal)) => refusal,
		other => panic!("expected a refusal, got {other:?}"),
	};

	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 1)]).await;
	drive(&pool, feed.clone(), async {
		assert!(eventually(|| async { row(&pool, &scope).await.is_some() }).await, "the scope was never registered");
		set_policy_kyc(&pool, &scope, 1).await;
		move_funds(Compartment::Main, compartment.clone(), "10", key()).await.expect("into an active scope");
		common::drain_to_quiescence(&relay, &pool).await;

		feed.lock().unwrap().push(changed(2, &scope, WireScopeState::Frozen, 2));
		assert!(eventually(|| async { row(&pool, &scope).await == Some(("frozen".into(), 2)) }).await, "the freeze never arrived");
		assert_eq!(refusal(move_funds(Compartment::Main, compartment.clone(), "1", key()).await), MoveRefusal::ScopeFrozen);
		move_funds(compartment.clone(), Compartment::Main, "4", key()).await.expect("out of a frozen scope");

		feed.lock().unwrap().push(changed(3, &scope, WireScopeState::Closed, 3));
		assert!(eventually(|| async { row(&pool, &scope).await == Some(("closed".into(), 3)) }).await, "the close never arrived");
		assert_eq!(refusal(move_funds(Compartment::Main, compartment.clone(), "1", key()).await), MoveRefusal::ScopeClosed);
		move_funds(compartment.clone(), Compartment::Main, "6", key()).await.expect("out of a closed scope");
		common::drain_to_quiescence(&relay, &pool).await;

		let posted = |key: LedgerAccountKey| {
			let ledger = ledger.clone();
			async move { Usdt::from_base_units(ledger.balance(&key).await.unwrap().posted) }
		};
		assert_eq!(posted(LedgerAccountKey::UserClaim(user)).await, usdt("20"), "everything came back to main");
		assert_eq!(posted(compartment.claim_key(user)).await, Usdt::ZERO);
	})
	.await;
}

async fn cursor_event_id(pool: &PgPool) -> Option<String> {
	sqlx::query_scalar("SELECT last_event_id FROM scope_events_cursor WHERE id = TRUE").fetch_one(pool).await.unwrap()
}

/// Proving the consumer did NOT do something needs a bound. Every cycle that gets past its
/// checks pulls at once, so a few poll intervals (50 ms in [`drive`]) is ample.
async fn settle() {
	tokio::time::sleep(Duration::from_millis(400)).await;
}

#[tokio::test]
async fn a_batch_records_the_event_the_cursor_stands_on_and_an_empty_page_its_pull() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	let last = changed(2, &scope, WireScopeState::Frozen, 2);
	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 1), last.clone()]).await;
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 2).await, "the cursor never passed the batch");
		assert_eq!(cursor_event_id(&pool).await, Some(last.event_id.clone()));
		// The idle pulls after the batch keep the stamp current.
		sqlx::query("UPDATE scope_events_cursor SET last_pulled_at = NULL WHERE id = TRUE").execute(&pool).await.unwrap();
		let stamped = || async {
			sqlx::query_scalar::<_, bool>("SELECT last_pulled_at IS NOT NULL FROM scope_events_cursor WHERE id = TRUE")
				.fetch_one(&pool)
				.await
				.unwrap()
		};
		assert!(eventually(stamped).await, "an empty page never stamped last_pulled_at");
		assert_eq!(cursor(&pool).await, 2, "an empty page does not move the cursor");
	})
	.await;
}

#[tokio::test]
async fn a_rewound_stream_holds_the_cursor_and_applies_nothing() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let (scope, after_rewind) = (fresh_scope(), fresh_scope());
	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 1), changed(2, &scope, WireScopeState::Active, 2)]).await;
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 2).await, "the cursor never passed the batch");
	})
	.await;

	// A restored concierge: the same positions hold other events, and there is more after.
	let rewound = FakeScopeEvents {
		feed: Arc::new(Mutex::new(vec![
			changed(1, &scope, WireScopeState::Active, 1),
			changed(2, &scope, WireScopeState::Active, 2),
			changed(3, &after_rewind, WireScopeState::Active, 1),
		])),
		..FakeScopeEvents::default()
	};
	let served = rewound.served.clone();
	drive_fake(&pool, rewound, async {
		assert!(
			eventually(|| async { served.lock().unwrap().contains(&(1, 1)) }).await,
			"the event under the cursor was never re-checked"
		);
		settle().await;
		assert!(!served.lock().unwrap().iter().any(|&(after, _)| after == 2), "the consumer pulled past a rewound stream");
		assert_eq!(cursor(&pool).await, 2);
		assert_eq!(row(&pool, &after_rewind).await, None, "nothing after the rewind is applied");
	})
	.await;
}

#[tokio::test]
async fn a_page_that_breaks_the_contract_is_not_applied() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	fresh_feed(&pool, Vec::new()).await;
	// Positions out of order: nothing in such a page can be trusted to be what it says.
	let canned = FakeScopeEvents {
		mode: Mode::Canned(PullScopeEventsResponse {
			events: vec![changed(2, &scope, WireScopeState::Active, 1), changed(1, &scope, WireScopeState::Frozen, 2)],
			next_position: 1,
		}),
		..FakeScopeEvents::default()
	};
	let served = canned.served.clone();
	drive_fake(&pool, canned, async {
		assert!(eventually(|| async { !served.lock().unwrap().is_empty() }).await, "the consumer never pulled");
		settle().await;
		assert_eq!(cursor(&pool).await, 0);
		assert_eq!(row(&pool, &scope).await, None, "an event of a malformed page was applied");
		let stamped: bool = sqlx::query_scalar("SELECT last_pulled_at IS NOT NULL FROM scope_events_cursor WHERE id = TRUE")
			.fetch_one(&pool)
			.await
			.unwrap();
		assert!(!stamped, "a malformed page is not a pull the stall alert may count");
	})
	.await;
}

#[tokio::test]
async fn a_failing_parked_replay_does_not_hold_back_the_stream() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Frozen, 1)]).await;
	sqlx::query("INSERT INTO scope_events_parked (position, event_id, scope, kind, reason) VALUES (40, 'parked-earlier', 'elsewhere', 99, 'test')")
		.execute(&pool)
		.await
		.unwrap();
	let fake = FakeScopeEvents {
		feed,
		mode: Mode::FailSingle,
		..FakeScopeEvents::default()
	};
	drive_fake(&pool, fake, async {
		assert!(caught_up(&pool, 1).await, "a failing replay held the stream back");
		assert_eq!(row(&pool, &scope).await, Some(("frozen".into(), 1)));
		let touched = || async {
			sqlx::query_scalar::<_, bool>("SELECT last_replay_at IS NOT NULL FROM scope_events_parked WHERE position = 40")
				.fetch_one(&pool)
				.await
				.unwrap()
		};
		assert!(eventually(touched).await, "the failed replay attempt was not recorded");
		assert_eq!(parked(&pool).await, vec![(40, "test".to_owned())], "it stays parked");
	})
	.await;
}

#[tokio::test]
async fn a_concierge_without_scope_events_is_a_warning_not_a_stall() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	fresh_feed(&pool, Vec::new()).await;
	let stall_after = Duration::from_secs(300);
	let old = || async {
		sqlx::query("UPDATE scope_events_cursor SET last_pulled_at = now() - interval '10 minutes', updated_at = now() - interval '1 day' WHERE id = TRUE")
			.execute(&pool)
			.await
			.unwrap();
	};
	let fake = FakeScopeEvents {
		mode: Mode::Unimplemented,
		..FakeScopeEvents::default()
	};
	drive_fake(&pool, fake, async {
		let noted = || async {
			sqlx::query_scalar::<_, bool>("SELECT last_unimplemented_at IS NOT NULL FROM scope_events_cursor WHERE id = TRUE")
				.fetch_one(&pool)
				.await
				.unwrap()
		};
		assert!(eventually(noted).await, "UNIMPLEMENTED was never recorded");
	})
	.await;

	old().await;
	assert!(matches!(freshness(&pool, stall_after).await.unwrap(), StreamFreshness::Unserved { lag_secs } if lag_secs >= 600));

	// The same lag with no UNIMPLEMENTED to explain it is a stall.
	sqlx::query("UPDATE scope_events_cursor SET last_unimplemented_at = NULL WHERE id = TRUE")
		.execute(&pool)
		.await
		.unwrap();
	assert!(matches!(freshness(&pool, stall_after).await.unwrap(), StreamFreshness::Stalled { .. }));

	// An UNIMPLEMENTED older than the threshold explains nothing now: concierge answers
	// something else, or nothing.
	sqlx::query("UPDATE scope_events_cursor SET last_unimplemented_at = now() - interval '6 minutes' WHERE id = TRUE")
		.execute(&pool)
		.await
		.unwrap();
	assert!(matches!(freshness(&pool, stall_after).await.unwrap(), StreamFreshness::Stalled { .. }));

	// A consumer that never got a page is measured from the cursor row's last write.
	sqlx::query("UPDATE scope_events_cursor SET last_pulled_at = NULL, last_unimplemented_at = NULL, updated_at = now() WHERE id = TRUE")
		.execute(&pool)
		.await
		.unwrap();
	assert!(matches!(freshness(&pool, stall_after).await.unwrap(), StreamFreshness::Fresh { .. }));
}

/// Postgres echoes a waiting backend's statement; the wait is observed, not timed.
async fn waiting_on(pool: &PgPool, wait_events: &[&str], query_like: &str) -> bool {
	sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND wait_event = ANY($1) AND query LIKE $2)")
		.bind(wait_events)
		.bind(query_like)
		.fetch_one(pool)
		.await
		.unwrap()
}

#[tokio::test]
async fn a_second_consumer_waits_for_the_lock_and_takes_over_when_it_is_released() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let scope = fresh_scope();
	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 1)]).await;
	let holder = singleton::try_acquire(&pool, singleton::keys::SCOPE_EVENTS)
		.await
		.unwrap()
		.expect("no consumer of another test still holds the lock");
	drive(&pool, feed, async {
		assert!(
			eventually(|| waiting_on(&pool, &["advisory"], "SELECT pg_advisory_lock%")).await,
			"the consumer never queued on the lock"
		);
		assert_eq!(cursor(&pool).await, 0, "a consumer without the lock followed the stream");
		holder.close().await.unwrap();
		assert!(caught_up(&pool, 1).await, "the waiting consumer never took over");
	})
	.await;
}

async fn funded_user(pool: &PgPool, relay: &Relay, notify: &Arc<Notify>, amount: &str) -> UserId {
	let users = PgUsers::new(pool.clone());
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user: UserId = users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(pool, user, 1).await;
	let tx_ref = TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	balance_app::record_deposit(&PgDeposits::new(pool.clone()), notify, tx_ref, user, Network::Bep20, Usdt::parse_decimal(amount).unwrap())
		.await
		.unwrap();
	common::drain_to_quiescence(relay, pool).await;
	user
}

/// A freeze that arrives while a move in is deciding waits for that move to commit, and the
/// next move reads it — the admission row lock against the consumer's `FOR UPDATE`.
#[tokio::test]
async fn a_freeze_waits_for_a_deciding_move_and_the_next_move_is_refused() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let _outbox = common::outbox_serial().await;
	let Some(ledger) = common::seeded_ledger(&pool, "scope events").await else { return };
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(piggybank_core::infrastructure::custody::StubCustody), notify.clone());
	let (store, outflow) = (PgCompartments::new(pool.clone()), PgOutflowPolicy::new(pool.clone()));
	let ports = CompartmentPorts {
		store: &store,
		ledger: ledger.as_ref(),
		policy: &outflow,
		relay: &notify,
	};
	let user = funded_user(&pool, &relay, &notify, "10").await;
	let scope = fresh_scope();
	let compartment = Compartment::parse(&scope).unwrap();

	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 1)]).await;
	drive(&pool, feed.clone(), async {
		assert!(caught_up(&pool, 1).await, "the scope was never registered");

		// What admission holds while it decides a move in.
		let mut deciding = pool.begin().await.unwrap();
		sqlx::query("SELECT 1 FROM scopes WHERE id = $1 FOR SHARE").bind(&scope).execute(&mut *deciding).await.unwrap();

		feed.lock().unwrap().push(changed(2, &scope, WireScopeState::Frozen, 2));
		// A row-lock wait shows as `transactionid` (the holder's xid) or, queued behind
		// another waiter, `tuple`.
		assert!(
			eventually(|| waiting_on(&pool, &["transactionid", "tuple"], "SELECT state, version FROM scopes WHERE id = $1 FOR UPDATE")).await,
			"the freeze never queued behind the deciding move"
		);
		assert_eq!(row(&pool, &scope).await, Some(("active".into(), 1)), "the freeze landed under a deciding move");
		deciding.commit().await.unwrap();

		assert!(eventually(|| async { row(&pool, &scope).await == Some(("frozen".into(), 2)) }).await, "the freeze never landed");
		let next = compartments_app::move_funds(
			&ports,
			user,
			Compartment::Main,
			compartment.clone(),
			Usdt::parse_decimal("1").unwrap(),
			&Uuid::new_v4().to_string(),
		)
		.await;
		assert!(matches!(next, Err(MoveError::Refused(MoveRefusal::ScopeFrozen))), "expected scope_frozen, got {next:?}");
	})
	.await;
}

/// The operator's step from docs/RUNBOOK-scope-policy.md.
async fn set_policy_kyc(pool: &PgPool, scope: &str, level: i32) {
	let updated = sqlx::query("UPDATE scope_policies SET min_kyc_level = $2, updated_at = now() WHERE scope = $1")
		.bind(scope)
		.bind(level)
		.execute(pool)
		.await
		.unwrap()
		.rows_affected();
	assert_eq!(updated, 1, "no policy row for {scope}");
}

/// A scope concierge announces takes no money in until an operator sets its policy; the
/// caller is told so up front, and money already in a scope always goes back to `main`.
#[tokio::test]
async fn a_new_scope_takes_no_money_in_until_its_policy_is_set() {
	let Some(pool) = common::pool().await else { return };
	let _serial = serial().await;
	let _outbox = common::outbox_serial().await;
	let Some(ledger) = common::seeded_ledger(&pool, "scope events").await else { return };
	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(piggybank_core::infrastructure::custody::StubCustody), notify.clone());
	let (store, outflow) = (PgCompartments::new(pool.clone()), PgOutflowPolicy::new(pool.clone()));
	let ports = CompartmentPorts {
		store: &store,
		ledger: ledger.as_ref(),
		policy: &outflow,
		relay: &notify,
	};
	let user = funded_user(&pool, &relay, &notify, "10").await;
	let scope = fresh_scope();
	let (id, compartment) = (ScopeId::parse(&scope).unwrap(), Compartment::parse(&scope).unwrap());
	let usdt = |raw: &str| Usdt::parse_decimal(raw).unwrap();
	let key = || Uuid::new_v4().to_string();

	let feed = fresh_feed(&pool, vec![changed(1, &scope, WireScopeState::Active, 1)]).await;
	drive(&pool, feed, async {
		assert!(caught_up(&pool, 1).await, "the scope was never registered");
		let view = compartments_app::get_scope_policy(&ports, user, &id).await.unwrap();
		assert!(!view.caller_eligible, "a scope with no policy set must not be offered");
		let refused = compartments_app::move_funds(&ports, user, Compartment::Main, compartment.clone(), usdt("4"), &key()).await;
		assert!(matches!(refused, Err(MoveError::Refused(MoveRefusal::KycLevel))), "expected kyc_level, got {refused:?}");

		set_policy_kyc(&pool, &scope, 1).await;
		assert!(compartments_app::get_scope_policy(&ports, user, &id).await.unwrap().caller_eligible);
		compartments_app::move_funds(&ports, user, Compartment::Main, compartment.clone(), usdt("4"), &key())
			.await
			.expect("into the scope once its policy admits the user");
		common::drain_to_quiescence(&relay, &pool).await;

		// Closing it again does not trap what is already inside.
		set_policy_kyc(&pool, &scope, CLOSED_UNTIL_SET_KYC).await;
		compartments_app::move_funds(&ports, user, compartment.clone(), Compartment::Main, usdt("4"), &key())
			.await
			.expect("out of a scope closed to moves in");
		common::drain_to_quiescence(&relay, &pool).await;
		assert_eq!(Usdt::from_base_units(ledger.balance(&LedgerAccountKey::UserClaim(user)).await.unwrap().posted), usdt("10"));
	})
	.await;
}
