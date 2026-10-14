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
	scopes::{Compartment, MoveRefusal},
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
	infrastructure::{compartments::PgCompartments, deposits::PgDeposits, outflow::PgOutflowPolicy, relay::Relay, scope_events::ScopeEventsConsumer, users::PgUsers},
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

/// Serves the feed by each event's own `position`, `> after_position`, capped by `limit`;
/// `next_position` is the last position returned — the unfiltered (bridge-token) reader's
/// contract. Refuses any other credential, as concierge does.
struct FakeScopeEvents {
	feed: Feed,
}

#[tonic::async_trait]
impl ScopeEvents for FakeScopeEvents {
	async fn pull_scope_events(&self, request: Request<PullScopeEventsRequest>) -> Result<Response<PullScopeEventsResponse>, Status> {
		match request.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
			Some(value) if value == format!("Bearer {BRIDGE_TOKEN}") => {}
			_ => return Err(Status::unauthenticated("bad bridge token")),
		}
		let req = request.into_inner();
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
	sqlx::query("UPDATE scope_events_cursor SET position = 0 WHERE id = TRUE").execute(pool).await.unwrap();
	sqlx::query("DELETE FROM scope_events_parked").execute(pool).await.unwrap();
	Arc::new(Mutex::new(events))
}

/// Run the fake server and the consumer beside `driver`; return once the driver has.
async fn drive<Fut: Future<Output = ()>>(pool: &PgPool, feed: Feed, driver: Fut) {
	let addr: SocketAddr = {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind an ephemeral port");
		listener.local_addr().unwrap()
	};
	let stop = CancellationToken::new();
	let server = {
		let stop = stop.clone();
		async move {
			Server::builder()
				.add_service(ScopeEventsServer::new(FakeScopeEvents { feed }))
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
		assert_eq!(policy, Some(0), "a registered scope gets its policy row at the defaults");
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
	let feed = fresh_feed(&pool, vec![unknown.clone(), changed(2, &beside, WireScopeState::Frozen, 1)]).await;
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
	let feed = Arc::new(Mutex::new(vec![readable, changed(2, &beside, WireScopeState::Frozen, 1)]));
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
