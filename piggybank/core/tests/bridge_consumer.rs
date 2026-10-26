//! Integration tests for the cross-plane lifecycle bridge consumer.
//!
//! These hit a **real** Postgres (no mocks, per the project rules) and stand up a fake
//! concierge `UserEvents` server in-process (a tonic server over a localhost socket) that
//! replays a fixed list of `UserLifecycleEvent`s. They run when `DATABASE_URL` is set and
//! skip otherwise. Each test uses a fresh random `auth_subject`, so runs neither collide nor
//! require a clean database.
//!
//! Concurrency is structured (no detached `tokio::spawn`): the fake server, the consumer, and
//! the asserting driver run as branches of one `tokio::join!`; the driver cancels a shared
//! token when its assertions are done, winding the other two branches down.

use std::{
	future::Future,
	net::SocketAddr,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use concierge_domain::authz::bank;
use evconcierge_contracts::concierge::v1::{
	PullUserLifecycleRequest, PullUserLifecycleResponse, SeatPermissions, UserLifecycleEvent,
	user_events_server::{UserEvents, UserEventsServer},
	user_lifecycle_event::Kind,
};
use piggybank_core::{
	infrastructure::{bridge, bridge::BridgeConsumer, outflow::PgOutflowPolicy, users::PgUsers},
	ports::{OutflowPolicy, UserRepository},
};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, transport::Server};

mod common;
use common::pool;

const BRIDGE_TOKEN: &str = "test-bridge-token";

/// The money-out block the gates read: the `frozen OR disabled` fold, through the same
/// port admission and dispatch use.
async fn blocked(pool: &PgPool, user_id: uuid::Uuid) -> bool {
	PgOutflowPolicy::new(pool.clone())
		.standing(domain::users::UserId::from_raw(user_id))
		.await
		.unwrap()
		.expect("the user has a banking row")
		.blocked
}
/// Fixed advisory-lock key serializing the two tests' drains over the single global cursor row.
const BRIDGE_TEST_LOCK: i64 = 0x4556_4252_4944_4745;

fn unique_subject() -> String {
	format!("itest-bridge-{}", uuid::Uuid::new_v4())
}

/// A fake concierge `UserEvents` server: serves a fixed ordered list, capped by `limit`,
/// with `position > after_position`. `position` here is the 1-based index into the list, so
/// the consumer's cursor semantics (next_position = max returned) are exercised faithfully.
/// Rejects a wrong/absent token with UNAUTHENTICATED, like the real server.
struct FakeUserEvents {
	events: Vec<UserLifecycleEvent>,
}

#[tonic::async_trait]
impl UserEvents for FakeUserEvents {
	async fn pull_user_lifecycle(&self, request: Request<PullUserLifecycleRequest>) -> Result<Response<PullUserLifecycleResponse>, Status> {
		match request.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
			Some(value) if value == format!("Bearer {BRIDGE_TOKEN}") => {}
			_ => return Err(Status::unauthenticated("bad bridge token")),
		}
		let req = request.into_inner();
		let limit = req.limit.max(1) as usize;
		let mut out = Vec::new();
		let mut next = req.after_position;
		for (idx, event) in self.events.iter().enumerate() {
			let position = idx as i64 + 1;
			if position > req.after_position {
				out.push(event.clone());
				next = position;
				if out.len() >= limit {
					break;
				}
			}
		}
		Ok(Response::new(PullUserLifecycleResponse { events: out, next_position: next }))
	}
}

fn event(subject: &str, kind: Kind, sequence: u64) -> UserLifecycleEvent {
	UserLifecycleEvent {
		user_id: uuid::Uuid::new_v4().to_string(),
		kind: kind as i32,
		kyc_level: 1,
		occurred_at: 0,
		event_id: uuid::Uuid::new_v4().to_string(),
		sequence,
		auth_subject: subject.to_string(),
		email: "bridged@example.com".into(),
		email_verified: true,
		token_version: 0,
		// Base events carry no role (empty → the consumer mirrors it as Investor); a
		// ROLE_CHANGED test overrides this field explicitly.
		role: String::new(),
		seat_permissions: None,
	}
}

/// A CREATED event carrying a known concierge user id — the handle the BFF later presents on
/// `IssueUserToken`, so the test can assert the bridge stores it and the resolve path finds it.
fn created_event(subject: &str, concierge_user_id: uuid::Uuid) -> UserLifecycleEvent {
	UserLifecycleEvent {
		user_id: concierge_user_id.to_string(),
		..event(subject, Kind::Created, 1)
	}
}

async fn user_id_for(pool: &PgPool, subject: &str) -> Option<uuid::Uuid> {
	sqlx::query_scalar("SELECT id FROM users WHERE auth_subject = $1")
		.bind(subject)
		.fetch_optional(pool)
		.await
		.unwrap()
}

/// Run the fake server (serving `events`) and the bridge consumer concurrently with the
/// `driver` future, all as branches of one `join!` — structured, no detached spawns. The
/// consumer drains the backlog into Postgres; `driver` waits for that, makes its assertions
/// against `pool`, then returns, after which the shared token cancels the server and consumer.
async fn drive<F, Fut>(pool: &PgPool, events: Vec<UserLifecycleEvent>, driver: F)
where
	F: FnOnce(PgPool) -> Fut,
	Fut: Future<Output = ()>, {
	// Bind once to claim a free ephemeral port, read it, then drop the listener and let tonic
	// re-bind the same addr — avoids a tokio-stream dep just to pass a pre-bound listener.
	// The bridge cursor is a single global row, so the two tests must not interleave their
	// drains. Serialize at the DB with a session advisory lock (held on a dedicated connection
	// for this drive) and reset the cursor so this drive pulls its own server from position 0.
	let mut guard = pool.acquire().await.expect("lock connection");
	sqlx::query("SELECT pg_advisory_lock($1)")
		.bind(BRIDGE_TEST_LOCK)
		.execute(guard.as_mut())
		.await
		.expect("take bridge test lock");
	sqlx::query("UPDATE bridge_cursor SET position = 0 WHERE id = TRUE").execute(pool).await.expect("reset cursor");

	let addr: SocketAddr = {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
		listener.local_addr().unwrap()
	};
	let stop = CancellationToken::new();

	let server = {
		let stop = stop.clone();
		async move {
			Server::builder()
				.add_service(UserEventsServer::new(FakeUserEvents { events }))
				.serve_with_shutdown(addr, stop.cancelled_owned())
				.await
				.ok();
		}
	};

	let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect_lazy();
	let consumer = BridgeConsumer::new(pool.clone(), channel, BRIDGE_TOKEN.to_string(), Duration::from_millis(50)).run(stop.clone());

	let asserter = {
		let pool = pool.clone();
		let stop = stop.clone();
		async move {
			// Let the server bind and the consumer poll/apply the backlog before asserting.
			tokio::time::sleep(Duration::from_millis(600)).await;
			driver(pool).await;
			stop.cancel();
		}
	};

	tokio::join!(server, consumer, asserter);

	// Release the session advisory lock (a pooled connection isn't closed, so it must be
	// unlocked explicitly) before the connection returns to the pool for the next drive.
	sqlx::query("SELECT pg_advisory_unlock($1)").bind(BRIDGE_TEST_LOCK).execute(guard.as_mut()).await.ok();
}

#[tokio::test]
async fn created_then_suspended_freezes_user_and_gates_money_op() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = unique_subject();
	let events = vec![event(&subject, Kind::Created, 1), event(&subject, Kind::Suspended, 2)];

	drive(&pool, events, |pool| async move {
		let user_id = user_id_for(&pool, &subject).await.expect("CREATED provisioned a banking user");
		assert!(blocked(&pool, user_id).await, "SUSPENDED must freeze the banking user — the money-op gate then rejects");
		// And the issuance resolve reports the freeze, so AuthService.IssueUserToken refuses to
		// mint a money-plane token for a suspended user (defense in depth beyond the op gate).
		let target = PgUsers::new(pool.clone())
			.resolve_issuance_by_banking_id(domain::users::UserId::from_raw(user_id))
			.await
			.unwrap()
			.expect("resolve issuance");
		assert!(target.disabled, "a suspended user resolves as disabled → no money token is issued");
	})
	.await;
}

#[tokio::test]
async fn banking_disable_blocks_money_ops_without_a_concierge_freeze() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();

	drive(&pool, vec![event(&subject, Kind::Created, 1)], |pool| async move {
		let user_id = user_id_for(&pool, &subject).await.expect("CREATED provisioned a banking user");
		// Not concierge-frozen, so nothing blocks money ops yet.
		assert!(!blocked(&pool, user_id).await, "a fresh user is not blocked");

		// A banking-side DisableUser sets status='disabled' and leaves `frozen` FALSE. The
		// money-op gate must still block it, matching the fold issuance already applies.
		sqlx::query("UPDATE users SET status = 'disabled' WHERE id = $1").bind(user_id).execute(&pool).await.unwrap();
		assert!(blocked(&pool, user_id).await, "a banking-disabled user must be blocked from money ops, like a concierge freeze");
	})
	.await;
}

#[tokio::test]
async fn role_changed_mirrors_role_onto_the_projection() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	// CREATED (empty role → Investor) then a ROLE_CHANGED promoting to admin. The money
	// plane's operator gate reads exactly this mirrored column.
	let mut promote = event(&subject, Kind::RoleChanged, 2);
	promote.role = "admin".to_string();
	let events = vec![event(&subject, Kind::Created, 1), promote];

	drive(&pool, events, move |pool| {
		let subject = subject.clone();
		async move {
			let role: String = sqlx::query_scalar("SELECT role FROM users WHERE auth_subject = $1")
				.bind(&subject)
				.fetch_one(&pool)
				.await
				.unwrap();
			assert_eq!(role, "admin", "ROLE_CHANGED mirrors the granted role onto the banking projection");
		}
	})
	.await;
}

async fn permissions_of(pool: &PgPool, subject: &str) -> Option<Vec<String>> {
	sqlx::query_scalar("SELECT permissions FROM users WHERE auth_subject = $1")
		.bind(subject)
		.fetch_one(pool)
		.await
		.unwrap()
}

/// `seat_permissions` is a snapshot on every row, like the role: a ROLE_CHANGED that only left
/// the set to a later PERMISSIONS_CHANGED would leave the two disagreeing in between, and a
/// parked row that replayed without its set would read as "holds nothing". Absent says
/// nothing and keeps the mirror; a stated empty set is a demotion and lands as one.
/// What lands is what the money-op gate then asks.
#[tokio::test]
async fn permissions_ride_every_kind_live_and_from_the_parking_lot() {
	let Some(pool) = pool().await else {
		return;
	};
	let set = |ps: &[&str]| ps.iter().map(|p| p.to_string()).collect::<Vec<_>>();
	let stated = |ps: &[&str]| Some(SeatPermissions { bank: set(ps) });
	let (created, promoted, redefined, orphan, kept, demoted) = (unique_subject(), unique_subject(), unique_subject(), unique_subject(), unique_subject(), unique_subject());
	let mut seated = event(&created, Kind::Created, 1);
	seated.seat_permissions = stated(&["bank:treasury:read"]);
	let mut promote = event(&promoted, Kind::RoleChanged, 2);
	promote.role = "admin".into();
	promote.seat_permissions = stated(&["bank:treasury:read", "bank:user_balance:read"]);
	let mut redefine = event(&redefined, Kind::PermissionsChanged, 2);
	redefine.seat_permissions = stated(&["bank:payment:open"]);
	let mut parked = event(&orphan, Kind::KycChanged, 2);
	parked.seat_permissions = stated(&["bank:user_balance:read"]);
	let mut keep = event(&kept, Kind::Created, 1);
	keep.seat_permissions = stated(&["bank:treasury:read"]);
	let mut admin_seat = event(&demoted, Kind::Created, 1);
	admin_seat.seat_permissions = stated(&["bank:payment:open"]);
	let mut demote = event(&demoted, Kind::RoleChanged, 2);
	demote.seat_permissions = stated(&[]);
	let events = vec![
		seated,
		event(&promoted, Kind::Created, 1),
		promote,
		event(&redefined, Kind::Created, 1),
		redefine,
		parked,
		keep,
		event(&kept, Kind::KycChanged, 2),
		admin_seat,
		demote,
	];
	let count = events.len() as i64;

	drive(&pool, events, move |pool| async move {
		assert_eq!(permissions_of(&pool, &created).await, Some(set(&["bank:treasury:read"])), "CREATED seats the row with its set");
		assert_eq!(
			permissions_of(&pool, &promoted).await,
			Some(set(&["bank:treasury:read", "bank:user_balance:read"])),
			"ROLE_CHANGED carries the set of the new seat"
		);
		assert_eq!(permissions_of(&pool, &redefined).await, Some(set(&["bank:payment:open"])));
		assert_eq!(permissions_of(&pool, &kept).await, Some(set(&["bank:treasury:read"])), "an unstated set keeps what is mirrored");
		assert_eq!(permissions_of(&pool, &demoted).await, Some(vec![]), "a stated empty set is a demotion");
		assert_eq!(cursor_position(&pool).await, count, "a kind this build names moves the cursor on");

		let gate = |subject: String| {
			let pool = pool.clone();
			async move {
				bridge::permissions_of(&pool, domain::users::UserId::from_raw(user_id_for(&pool, &subject).await.expect("provisioned")))
					.await
					.unwrap()
			}
		};
		let admin = gate(promoted.clone()).await;
		assert!(admin.may(bank::UserBalance::Read), "the mirrored set admits what it names");
		assert!(!admin.may(bank::Payment::Open), "and refuses what it does not, whatever the role says");
		assert!(gate(redefined.clone()).await.may(bank::Payment::Open));
		assert!(!gate(demoted.clone()).await.may(bank::Payment::Open));

		PgUsers::new(pool.clone())
			.provision(
				domain::auth::AuthSubject::parse(&orphan).unwrap(),
				domain::users::Email::parse("bridged@example.com").unwrap(),
				true,
			)
			.await
			.expect("first sign-in provisions the row");
		let replayed = eventually(|| {
			let (pool, orphan) = (pool.clone(), orphan.clone());
			async move { permissions_of(&pool, &orphan).await == Some(vec!["bank:user_balance:read".into()]) }
		})
		.await;
		assert!(replayed, "the parked set replays onto the row");
	})
	.await;
}

#[tokio::test]
async fn created_stores_concierge_id_and_resolves_for_issuance() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	let concierge_id = uuid::Uuid::new_v4();

	drive(&pool, vec![created_event(&subject, concierge_id)], move |pool| {
		let subject = subject.clone();
		async move {
			// The bridge stored the concierge user id on the mirror row — the issuance handle.
			let stored: Option<uuid::Uuid> = sqlx::query_scalar("SELECT concierge_user_id FROM users WHERE auth_subject = $1")
				.bind(&subject)
				.fetch_one(&pool)
				.await
				.unwrap();
			assert_eq!(stored, Some(concierge_id), "CREATED must store the concierge user id");

			// The issuance resolve path (AuthService.IssueUserToken → ResolveForIssuance) finds the
			// user by that concierge id, and the refresh path resolves the same row by hub id.
			let users = PgUsers::new(pool.clone());
			let by_concierge = users
				.resolve_issuance_by_concierge_id(domain::users::ConciergeUserId::from_raw(concierge_id))
				.await
				.unwrap()
				.expect("resolved by concierge id");
			assert!(!by_concierge.disabled, "a freshly created user is not disabled");
			let by_banking = users.resolve_issuance_by_banking_id(by_concierge.user_id).await.unwrap().expect("resolved by hub id");
			assert_eq!(by_banking.user_id, by_concierge.user_id, "both lookups resolve the same hub user");
		}
	})
	.await;
}

#[tokio::test]
async fn redelivery_is_idempotent() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	// CREATED, SUSPENDED, then a stale REINSTATED with a LOWER sequence than the suspend —
	// the per-user order guard must drop it, so the user stays frozen.
	let events = vec![event(&subject, Kind::Created, 1), event(&subject, Kind::Suspended, 5), event(&subject, Kind::Reinstated, 3)];

	// First pass: apply the backlog. The stale lower-sequence reinstate must be dropped.
	let subject_a = subject.clone();
	drive(&pool, events.clone(), |pool| async move {
		let user_id = user_id_for(&pool, &subject_a).await.expect("provisioned");
		let seq: i64 = sqlx::query_scalar("SELECT last_lifecycle_sequence FROM users WHERE id = $1")
			.bind(user_id)
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(seq, 5, "applied through the suspend; the stale lower-sequence reinstate is dropped");
		assert!(blocked(&pool, user_id).await, "stale REINSTATED must not un-freeze");
	})
	.await;

	// Second pass: `drive` resets the cursor to 0, so a fresh consumer re-pulls and re-applies
	// the SAME events — dedupe by per-user sequence must make every re-apply a no-op (frozen
	// stays frozen, sequence stays 5).
	drive(&pool, events, |pool| async move {
		let user_id = user_id_for(&pool, &subject).await.expect("provisioned");
		let seq: i64 = sqlx::query_scalar("SELECT last_lifecycle_sequence FROM users WHERE id = $1")
			.bind(user_id)
			.fetch_one(&pool)
			.await
			.unwrap();
		assert_eq!(seq, 5, "redelivery is a no-op — sequence does not move");
		assert!(blocked(&pool, user_id).await, "redelivery keeps the user frozen");
	})
	.await;
}

/// THE MIRRORED COLUMN IS THE ONLY SOURCE OF A PERMISSION.
///
/// The money plane used to promote subjects listed in an `ADMIN_SUBJECTS` env var to
/// `Role::Owner` inside `require_permission`, ahead of this lookup. That produced a second
/// UUID list an operator had to keep in sync with concierge's by hand. The override is gone: a subject
/// with no row in the local projection holds nothing — least of all the permission that
/// governs the treasury.
#[tokio::test]
async fn a_subject_absent_from_the_projection_is_never_an_owner() {
	let Some(pool) = pool().await else {
		return;
	};
	// Never provisioned by the bridge, so no `users` row exists for it.
	let stranger = domain::users::UserId::from_raw(uuid::Uuid::new_v4());

	let held = bridge::permissions_of(&pool, stranger).await.unwrap();
	assert!(
		!held.may(bank::Treasury::Read) && !held.may(bank::Consilium::Manage),
		"no local row ⇒ nothing, not an inherited privilege"
	);
}

/// Poll `check` until it holds or the budget runs out, then return whether it held. The
/// consumer runs concurrently on its own poll interval, so an assertion about work it has
/// yet to do needs to wait for a cycle rather than race one.
async fn eventually<F, Fut>(mut check: F) -> bool
where
	F: FnMut() -> Fut,
	Fut: Future<Output = bool>, {
	for _ in 0..40 {
		if check().await {
			return true;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	false
}

async fn parked_count(pool: &PgPool, subject: &str) -> i64 {
	sqlx::query_scalar("SELECT count(*) FROM bridge_deferred_event WHERE auth_subject = $1")
		.bind(subject)
		.fetch_one(pool)
		.await
		.unwrap()
}

async fn cursor_position(pool: &PgPool) -> i64 {
	sqlx::query_scalar("SELECT position FROM bridge_cursor WHERE id = TRUE").fetch_one(pool).await.unwrap()
}

async fn sequence_of(pool: &PgPool, subject: &str) -> i64 {
	sqlx::query_scalar("SELECT last_lifecycle_sequence FROM users WHERE auth_subject = $1")
		.bind(subject)
		.fetch_one(pool)
		.await
		.unwrap()
}

/// THE TIER MIRROR, END TO END — the path that had no test at all.
///
/// `users.kyc_level` is what every money gate reads (`application::wallet::is_verified`,
/// the withdrawal admission, the payout standing). Nothing else writes it: the concierge
/// plane owns the tier and this event is the only way it crosses. A regression here does
/// not fail loudly — it leaves users at the schema default of 0, silently locked out of
/// deposit addresses and withdrawals.
#[tokio::test]
async fn kyc_changed_mirrors_the_tier_the_money_gates_read() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	// A bare registration (level 0 — a confirmed email and nothing else), verified afterwards.
	let mut registered = event(&subject, Kind::Created, 1);
	registered.kyc_level = 0;
	let mut verified = event(&subject, Kind::KycChanged, 2);
	verified.kyc_level = domain::users::KYC_LEVEL_VERIFIED;
	let events = vec![registered, verified];

	drive(&pool, events, move |pool| {
		let subject = subject.clone();
		async move {
			let user_id = user_id_for(&pool, &subject).await.expect("CREATED provisioned a banking user");
			let level: i32 = sqlx::query_scalar("SELECT kyc_level FROM users WHERE id = $1").bind(user_id).fetch_one(&pool).await.unwrap();
			assert_eq!(level as u32, domain::users::KYC_LEVEL_VERIFIED, "KYC_CHANGED mirrors the tier onto the banking projection");

			// And the gates read it back through the repository they actually use, not the
			// column directly — an aggregate that dropped the field on the way out would still
			// leave the user locked out.
			let account = PgUsers::new(pool.clone())
				.find_by_id(domain::users::UserId::from_raw(user_id))
				.await
				.unwrap()
				.expect("the mirrored user loads");
			assert!(
				account.kyc_level() >= domain::users::KYC_LEVEL_VERIFIED,
				"the mirrored tier must clear the verification gate the wallet and withdrawal paths apply"
			);
		}
	})
	.await;
}

/// AN EVENT FOR A SUBJECT BANKING HAS NEVER SEEN MUST SURVIVE UNTIL IT CAN BE APPLIED.
///
/// The cursor is global and the concierge only ever re-delivers ahead of it, so an event
/// the consumer walks past is gone for good. `apply` used to walk past every event whose
/// subject had no local row — a user who has not signed in here yet, or anyone at all if
/// banking was deployed after the concierge and their CREATED has already aged out of the
/// outbox. The tier below would have vanished and left the user at level 0, locked out of
/// both a deposit address and a withdrawal, until the concierge happened to move their
/// tier again.
#[tokio::test]
async fn an_event_for_an_unknown_subject_is_parked_and_replayed_when_the_row_appears() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	// No CREATED — only the tier change reaches this deployment.
	let mut verified = event(&subject, Kind::KycChanged, 2);
	verified.kyc_level = domain::users::KYC_LEVEL_VERIFIED;

	drive(&pool, vec![verified], move |pool| {
		let subject = subject.clone();
		async move {
			assert!(user_id_for(&pool, &subject).await.is_none(), "nothing has provisioned this subject");
			assert_eq!(parked_count(&pool, &subject).await, 1, "an event for an unknown subject is parked, not consumed into the void");
			// The cursor is deliberately NOT held here: the event is durable on this side, so
			// one orphan subject must not wedge the mirror for every other user.
			assert_eq!(cursor_position(&pool).await, 1, "parking keeps the stream moving");

			// First sign-in materializes the row — at the schema default of level 0, which is
			// exactly the lock-out the dropped event used to leave behind.
			PgUsers::new(pool.clone())
				.provision(
					domain::auth::AuthSubject::parse(&subject).unwrap(),
					domain::users::Email::parse("bridged@example.com").unwrap(),
					true,
				)
				.await
				.expect("first sign-in provisions the row");

			let mirrored = eventually(|| {
				let (pool, subject) = (pool.clone(), subject.clone());
				async move {
					let level: i32 = sqlx::query_scalar("SELECT kyc_level FROM users WHERE auth_subject = $1")
						.bind(&subject)
						.fetch_one(&pool)
						.await
						.unwrap();
					level as u32 == domain::users::KYC_LEVEL_VERIFIED
				}
			})
			.await;
			assert!(mirrored, "the parked tier must be replayed onto the row the moment it exists");
			assert_eq!(parked_count(&pool, &subject).await, 0, "a replayed event is released from the parking lot");
		}
	})
	.await;
}

/// A KIND THIS BUILD CANNOT NAME MUST NOT BE RECORDED AS APPLIED.
///
/// The concierge ships ahead of banking, so a kind newer than the pinned contracts is the
/// ordinary shape of a mid-rollout event, not a corruption. Bumping `last_lifecycle_sequence`
/// for it — the old "so it isn't re-fetched forever" behaviour — put the event behind the
/// per-user guard, where the upgraded build could never reach it: if the new kind meant a
/// freeze or a tier revocation, the money plane would go on trading under rules the identity
/// plane had already withdrawn. Nothing local can interpret it, so the cursor stops instead.
#[tokio::test]
async fn an_unreadable_kind_holds_the_cursor_until_a_build_that_understands_it() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	// A kind number outside this build's enum, exactly as an older binary would decode one.
	let mut from_the_future = event(&subject, Kind::Suspended, 2);
	from_the_future.kind = 99;
	// A third event BEHIND the unreadable one: it must not be applied either, or it would
	// advance the per-user guard past the event the stop exists to preserve.
	let mut later = event(&subject, Kind::KycChanged, 3);
	later.kyc_level = domain::users::KYC_LEVEL_VERIFIED;
	// Registered but not yet verified, so the tier the stop withholds is visibly absent.
	let mut registered = event(&subject, Kind::Created, 1);
	registered.kyc_level = 0;
	let events = vec![registered, from_the_future, later];

	let before = subject.clone();
	drive(&pool, events.clone(), move |pool| {
		let subject = before;
		async move {
			assert_eq!(sequence_of(&pool, &subject).await, 1, "the unreadable event must not be marked applied");
			assert_eq!(cursor_position(&pool).await, 0, "and the batch carrying it must not be consumed");
			assert_eq!(parked_count(&pool, &subject).await, 0, "an unreadable event is not parked — only a newer binary can read it");
			let level: i32 = sqlx::query_scalar("SELECT kyc_level FROM users WHERE auth_subject = $1")
				.bind(&subject)
				.fetch_one(&pool)
				.await
				.unwrap();
			assert_ne!(level as u32, domain::users::KYC_LEVEL_VERIFIED, "nothing behind the stop is applied either");
		}
	})
	.await;

	// The upgrade: the same outbox rows, at the same positions, decoded by a build that now
	// names the kind. Because nothing above was consumed, they are all still on offer.
	let mut understood = events;
	understood[1].kind = Kind::Suspended as i32;
	drive(&pool, understood, move |pool| {
		let subject = subject.clone();
		async move {
			let user_id = user_id_for(&pool, &subject).await.expect("provisioned by the first pass");
			assert!(blocked(&pool, user_id).await, "the freeze that was unreadable before must land after the upgrade");
			assert_eq!(sequence_of(&pool, &subject).await, 3, "and the events behind it apply in order");
			assert_eq!(cursor_position(&pool).await, 3, "the whole batch is consumed once every event in it is readable");
		}
	})
	.await;
}

async fn email_of(pool: &PgPool, subject: &str) -> (String, bool) {
	sqlx::query_as("SELECT email, email_verified FROM users WHERE auth_subject = $1")
		.bind(subject)
		.fetch_one(pool)
		.await
		.unwrap()
}

/// AN EMAIL CHANGE HAS NO EVENT OF ITS OWN — THE SNAPSHOT IS THE WHOLE DELIVERY MECHANISM.
///
/// concierge's `change_email` bumps `row_version` without `bump_and_emit`, on the stated
/// contract that banking re-reads the address from the snapshot on the next lifecycle event.
/// Banking never did: `email` was written once, by the CREATED insert, under
/// `ON CONFLICT DO NOTHING`, and no other arm touched it — so a user who changed their
/// address at the IdP kept their old one on the money plane forever, on statements, notices
/// and every operator screen (EV-invest/concierge#46).
#[tokio::test]
async fn a_changed_address_arrives_on_the_next_lifecycle_event() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = unique_subject();
	let mut registered = event(&subject, Kind::Created, 1);
	registered.email = "before@example.com".into();
	// The address changed at the IdP between the two; only the KYC move is emitted, and it
	// carries the new address as part of its snapshot.
	let mut verified = event(&subject, Kind::KycChanged, 2);
	verified.email = "after@example.com".into();
	verified.kyc_level = domain::users::KYC_LEVEL_VERIFIED;

	drive(&pool, vec![registered, verified], move |pool| {
		let subject = subject.clone();
		async move {
			let (email, _) = email_of(&pool, &subject).await;
			assert_eq!(email, "after@example.com", "the snapshot on a non-CREATED event must refresh the mirrored address");

			// And the aggregate still loads: a mirrored address that no longer parses would
			// take the user's whole profile and money surface down with it.
			let user_id = user_id_for(&pool, &subject).await.expect("provisioned");
			let account = PgUsers::new(pool.clone())
				.find_by_id(domain::users::UserId::from_raw(user_id))
				.await
				.unwrap()
				.expect("the mirrored user loads");
			assert_eq!(account.email().as_str(), "after@example.com", "the repository reads back the refreshed address");
		}
	})
	.await;
}

async fn credential_changed(pool: &PgPool, subject: &str) -> bool {
	sqlx::query_scalar("SELECT credential_changed_at IS NOT NULL FROM users WHERE auth_subject = $1")
		.bind(subject)
		.fetch_one(pool)
		.await
		.unwrap()
}

/// The new-address cooling-off clock starts on the two credential signals concierge gives: a
/// changed address and a raised revoke floor. An ordinary event, and a floor that did not rise,
/// start nothing. (A passkey or password change that revokes no session has no event, so it is
/// the one change this cannot see.)
#[tokio::test]
async fn a_changed_address_or_a_raised_revoke_floor_marks_a_credential_change() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let (by_email, by_revoke) = (unique_subject(), unique_subject());
	let with_email = |subject: &str, kind: Kind, sequence: u64, email: &str, token_version: u64| {
		let mut e = event(subject, kind, sequence);
		e.email = email.into();
		e.token_version = token_version;
		e
	};
	let before = |a: &str, b: &str| {
		vec![
			with_email(a, Kind::Created, 1, "same@example.com", 0),
			with_email(a, Kind::KycChanged, 2, "same@example.com", 0),
			with_email(b, Kind::Created, 1, "revoker@example.com", 0),
			with_email(b, Kind::SessionsRevoked, 2, "revoker@example.com", 0),
		]
	};
	let (a, b) = (by_email.clone(), by_revoke.clone());
	drive(&pool, before(&a, &b), |pool| async move {
		assert!(!credential_changed(&pool, &a).await, "a snapshot that did not change starts nothing");
		assert!(!credential_changed(&pool, &b).await, "a revoke that did not raise the floor starts nothing");
	})
	.await;

	let mut later = before(&by_email, &by_revoke);
	later.push(with_email(&by_email, Kind::KycChanged, 3, "moved@example.com", 0));
	later.push(with_email(&by_revoke, Kind::SessionsRevoked, 3, "revoker@example.com", 4));
	drive(&pool, later, |pool| async move {
		assert!(credential_changed(&pool, &by_email).await, "a changed address starts the clock");
		assert!(credential_changed(&pool, &by_revoke).await, "a raised floor starts the clock");
	})
	.await;
}

/// The verification flag rides the same snapshot, and on a kind that has nothing to do with
/// either — the refresh is kind-independent by construction, not a KYC_CHANGED special case.
#[tokio::test]
async fn the_verification_flag_travels_with_the_address() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	let mut registered = event(&subject, Kind::Created, 1);
	registered.email = "unconfirmed@example.com".into();
	registered.email_verified = false;
	// A revoke says nothing about the address — but it carries the snapshot, so it delivers it.
	let mut revoked = event(&subject, Kind::SessionsRevoked, 2);
	revoked.email = "confirmed@example.com".into();
	revoked.email_verified = true;

	drive(&pool, vec![registered, revoked], move |pool| {
		let subject = subject.clone();
		async move {
			assert_eq!(
				email_of(&pool, &subject).await,
				("confirmed@example.com".to_string(), true),
				"any lifecycle kind carrying the snapshot refreshes both the address and its verification flag"
			);
		}
	})
	.await;
}

/// A PARKED EVENT CARRIES A SNAPSHOT FROM BEFORE THE ROW EXISTED, AND MUST NOT WRITE IT BACK.
///
/// Parking (migration 0028) means the subject had NO local row when the event arrived. The
/// only thing that creates one afterwards is a first sign-in, which writes the IdP's live
/// address — so by the time the replay runs, the row is strictly newer than the snapshot the
/// replay is holding. Mirroring it anyway would roll the address back to whatever it was at
/// the moment concierge drained the event, which for a bootstrap backlog can be months.
#[tokio::test]
async fn a_replayed_parked_event_does_not_roll_the_address_back() {
	let Some(pool) = pool().await else {
		return;
	};
	let subject = unique_subject();
	// No CREATED reaches this deployment — only a tier change, snapshotting the OLD address.
	let mut verified = event(&subject, Kind::KycChanged, 2);
	verified.email = "stale@example.com".into();
	verified.kyc_level = domain::users::KYC_LEVEL_VERIFIED;

	drive(&pool, vec![verified], move |pool| {
		let subject = subject.clone();
		async move {
			assert_eq!(parked_count(&pool, &subject).await, 1, "an event for an unknown subject is parked");

			// First sign-in materializes the row with the address the IdP holds NOW.
			PgUsers::new(pool.clone())
				.provision(
					domain::auth::AuthSubject::parse(&subject).unwrap(),
					domain::users::Email::parse("current@example.com").unwrap(),
					true,
				)
				.await
				.expect("first sign-in provisions the row");

			let replayed = eventually(|| {
				let (pool, subject) = (pool.clone(), subject.clone());
				async move { parked_count(&pool, &subject).await == 0 }
			})
			.await;
			assert!(replayed, "the parked event must be released once the row exists");

			let (email, _) = email_of(&pool, &subject).await;
			assert_eq!(email, "current@example.com", "a replayed snapshot must not overwrite the newer address the row already holds");
			let level: i32 = sqlx::query_scalar("SELECT kyc_level FROM users WHERE auth_subject = $1")
				.bind(&subject)
				.fetch_one(&pool)
				.await
				.unwrap();
			assert_eq!(
				level as u32,
				domain::users::KYC_LEVEL_VERIFIED,
				"the state-machine fields it carries still apply — only the snapshot is withheld"
			);
		}
	})
	.await;
}

// ── #181: the ways a poll cycle can misbehave ────────────────────────────────────────
//
// These drive the consumer against a deliberately broken server. Two of them are about the
// SHAPE of the retry rather than about an event being mirrored, and watch WHEN the consumer
// comes back: each measures the time it took to make a fixed number of pulls and asserts a
// LOWER bound on it. That direction is deliberate: a loaded machine can only stretch those
// gaps, so the assertion cannot fail because the suite ran beside something else — the price
// is that enough load could hide a regression, which is the right way round for a test that
// would otherwise be one more flake in this repository.
//
// [`a_cycle_that_mirrored_a_batch_does_not_back_off`] is the exception, and has to be: what
// it forbids is the consumer being SLOWER than it should, so its bound is an upper one. It
// is safe by margin instead of by direction — the behaviour it rejects spends 25 s in timer
// sleeps before the pull it waits for (and those can only stretch), while the behaviour it
// accepts needs a couple of seconds of real work against a cap of [`OBSERVE_CAP`].
//
// They share [`drive_misbehaving`] rather than [`drive`]: a consumer stuck in a hot loop
// never reaches the `select!` that observes the shutdown token, so the consumer branch has
// to be bounded by a timeout — otherwise a regression would hang the suite instead of
// failing it, and a hang says nothing about which invariant broke.

/// When each pull reached the fake server. The critical section holds no `.await`, so a
/// `std` mutex is the right one.
type PullLog = Arc<Mutex<Vec<Instant>>>;

fn pull_log() -> PullLog {
	Arc::new(Mutex::new(Vec::new()))
}

/// Stamps this pull and returns how many have arrived, counting it — which is how
/// [`FlakyUserEvents`] decides whether to serve this one or fail it.
fn record(log: &PullLog) -> usize {
	let mut stamps = log.lock().expect("pull log");
	stamps.push(Instant::now());
	stamps.len()
}

/// Elapsed time from the first pull to the `n`th, or `None` when fewer than `n` arrived.
fn span_of_first(log: &PullLog, n: usize) -> Option<Duration> {
	let stamps = log.lock().expect("pull log");
	(stamps.len() >= n).then(|| stamps[n - 1] - stamps[0])
}

/// How long the consumer branch is allowed to take after cancellation, and how long
/// [`drive_misbehaving`] waits for the pulls it was asked to observe. Generous: every real
/// assertion here is a lower bound, so the only thing these bound is a hang.
const CONSUMER_GRACE: Duration = Duration::from_secs(30);
const OBSERVE_CAP: Duration = Duration::from_secs(20);

/// A server that breaks the pull contract: it hands back events while leaving
/// `next_position` at the cursor the consumer sent, so the next pull asks the same question
/// and gets the same answer.
///
/// `full` picks the batch size, because the two tests here want opposite things from it. A
/// FULL batch is what makes `drain` loop for more, and therefore what a hot loop is made of;
/// a single event lets `drain` stop on its own, so what is left to observe is whether the
/// batch was applied on the way out.
struct StuckUserEvents {
	event: UserLifecycleEvent,
	full: bool,
	pulls: PullLog,
}

#[tonic::async_trait]
impl UserEvents for StuckUserEvents {
	async fn pull_user_lifecycle(&self, request: Request<PullUserLifecycleRequest>) -> Result<Response<PullUserLifecycleResponse>, Status> {
		record(&self.pulls);
		let req = request.into_inner();
		// `limit` is what the consumer asked for, so a batch of that size is a full one
		// whatever `PULL_LIMIT` happens to be — the test does not need to know it.
		let count = if self.full { req.limit.max(1) as usize } else { 1 };
		Ok(Response::new(PullUserLifecycleResponse {
			events: vec![self.event.clone(); count],
			next_position: req.after_position,
		}))
	}
}

/// A concierge that is simply unreachable for this consumer: every pull is an error, the
/// way a restarting pod or a rejected token looks from here.
struct FailingUserEvents {
	pulls: PullLog,
}

#[tonic::async_trait]
impl UserEvents for FailingUserEvents {
	async fn pull_user_lifecycle(&self, _request: Request<PullUserLifecycleRequest>) -> Result<Response<PullUserLifecycleResponse>, Status> {
		record(&self.pulls);
		Err(Status::unavailable("concierge is down"))
	}
}

/// A concierge that serves one full batch and then dies, over and over: odd pulls come back
/// with a batch that moves the cursor, even pulls — the one `drain` makes for the rest of
/// the backlog — fail. That is what a rolling restart looks like from here, and equally a
/// pair of replicas one of which is not ready, or a per-request timeout only some pulls
/// trip. Every cycle mirrors and commits a batch and then fails: the bridge is working.
struct FlakyUserEvents {
	event: UserLifecycleEvent,
	pulls: PullLog,
}

#[tonic::async_trait]
impl UserEvents for FlakyUserEvents {
	async fn pull_user_lifecycle(&self, request: Request<PullUserLifecycleRequest>) -> Result<Response<PullUserLifecycleResponse>, Status> {
		let nth = record(&self.pulls);
		let req = request.into_inner();
		if nth.is_multiple_of(2) {
			return Err(Status::unavailable("concierge went away mid-drain"));
		}
		let count = req.limit.max(1);
		Ok(Response::new(PullUserLifecycleResponse {
			events: vec![self.event.clone(); count as usize],
			next_position: req.after_position + i64::from(count),
		}))
	}
}

/// Run `service` against the real consumer at `poll` until `pulls` holds `want` entries (or
/// [`OBSERVE_CAP`] passes), then cancel and wind down. Returns whether the consumer
/// actually finished within [`CONSUMER_GRACE`] of being told to stop.
///
/// Deliberately takes no assertion closure: the bridge cursor is one global row, so this
/// holds the suite's advisory lock for the drive, and an assertion that unwound inside the
/// `join!` would leave that lock on a pooled connection and wedge every later test. The
/// caller asserts afterwards, on the log it passed in and on the database.
async fn drive_misbehaving<S: UserEvents>(pool: &PgPool, service: S, poll: Duration, pulls: PullLog, want: usize) -> bool {
	let mut guard = pool.acquire().await.expect("lock connection");
	sqlx::query("SELECT pg_advisory_lock($1)")
		.bind(BRIDGE_TEST_LOCK)
		.execute(guard.as_mut())
		.await
		.expect("take bridge test lock");
	sqlx::query("UPDATE bridge_cursor SET position = 0 WHERE id = TRUE").execute(pool).await.expect("reset cursor");

	let addr: SocketAddr = {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
		listener.local_addr().unwrap()
	};
	let stop = CancellationToken::new();

	let server = {
		let stop = stop.clone();
		async move {
			Server::builder()
				.add_service(UserEventsServer::new(service))
				.serve_with_shutdown(addr, stop.cancelled_owned())
				.await
				.ok();
		}
	};

	let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect_lazy();
	let consumer = {
		let pool = pool.clone();
		let stop = stop.clone();
		async move {
			let run = BridgeConsumer::new(pool, channel, BRIDGE_TOKEN.to_string(), poll).run(stop);
			tokio::time::timeout(CONSUMER_GRACE, run).await.is_ok()
		}
	};

	let stopper = {
		let stop = stop.clone();
		let pulls = pulls.clone();
		async move {
			let deadline = Instant::now() + OBSERVE_CAP;
			while span_of_first(&pulls, want).is_none() && Instant::now() < deadline {
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
			stop.cancel();
		}
	};

	let (_, finished, ()) = tokio::join!(server, consumer, stopper);

	sqlx::query("SELECT pg_advisory_unlock($1)").bind(BRIDGE_TEST_LOCK).execute(guard.as_mut()).await.ok();
	finished
}

/// A CURSOR THAT DOES NOT MOVE MUST END THE DRAIN, NOT RESTART IT.
///
/// `drain` keeps pulling while the server returns a full batch, and the only thing that
/// makes the next pull different is `next_position` moving past the stored cursor. The
/// server's contract (`position > after_position`) guarantees it does, so this is a
/// defence against a contract change rather than a live bug — but the failure mode it
/// defends against is a tight loop against the identity plane, diagnosable only as an
/// unexplained load spike, and the consumer in that state never reaches the `select!` that
/// would let it shut down (#181).
#[tokio::test]
async fn a_pull_that_cannot_move_the_cursor_ends_the_drain_instead_of_spinning() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = unique_subject();
	// A row for the batch to land on, already past the event's sequence — so even the
	// unguarded consumer's apply is a no-op and what the test measures is the looping,
	// not the mirroring.
	PgUsers::new(pool.clone())
		.provision(
			domain::auth::AuthSubject::parse(&subject).unwrap(),
			domain::users::Email::parse("stuck@example.com").unwrap(),
			true,
		)
		.await
		.expect("provision the subject the stuck batch names");
	sqlx::query("UPDATE users SET last_lifecycle_sequence = 9 WHERE auth_subject = $1")
		.bind(&subject)
		.execute(&pool)
		.await
		.unwrap();

	// One second apart is a gap nothing else in this test can manufacture: a consumer that
	// ends the drain waits the poll interval before asking again, while one that re-pulls
	// the batch it cannot move past is paced only by how long applying it takes.
	const POLL: Duration = Duration::from_secs(1);
	const WANT: usize = 3;
	let pulls = pull_log();
	let finished = drive_misbehaving(
		&pool,
		StuckUserEvents {
			event: event(&subject, Kind::SessionsRevoked, 1),
			full: true,
			pulls: pulls.clone(),
		},
		POLL,
		pulls.clone(),
		WANT,
	)
	.await;

	assert!(finished, "the consumer winds down on cancellation");
	let span = span_of_first(&pulls, WANT).expect("the consumer pulled at least three times");
	// Two poll intervals is 2 s; a spinning drain covers three pulls in a fraction of one.
	// (Cancellation alone proves nothing here: it stops the fake server, the next pull then
	// fails, and even a spinning `drain` returns on that error.)
	assert!(
		span >= Duration::from_millis(1500),
		"the drain re-pulled the batch it could not move past instead of ending — {WANT} pulls inside {span:?} at a {POLL:?} poll"
	);
	assert_eq!(
		cursor_position(&pool).await,
		0,
		"nothing is consumed on a cursor that cannot move: the batch stays in concierge's outbox for a build that can move past it"
	);
}

/// A CYCLE THAT KEEPS FAILING MUST SLOW DOWN.
///
/// The poll interval is a *retry* interval while concierge is unreachable, and at the
/// production value that is a permanent trickle of failing connects against the plane that
/// is already down, plus a WARN per attempt in the log of the hub. The delay doubles per
/// consecutive failure instead, up to a minute (#181).
#[tokio::test]
async fn a_failing_cycle_backs_off_instead_of_retrying_at_the_poll_interval() {
	let Some(pool) = pool().await else {
		return;
	};
	// Seven attempts at a 50 ms poll: 0.3 s if every retry waits the interval, 3.1 s once
	// the wait doubles each time (50 + 100 + 200 + 400 + 800 + 1600).
	const POLL: Duration = Duration::from_millis(50);
	const WANT: usize = 7;
	let pulls = pull_log();
	let finished = drive_misbehaving(&pool, FailingUserEvents { pulls: pulls.clone() }, POLL, pulls.clone(), WANT).await;

	assert!(finished, "a backing-off consumer still winds down on cancellation rather than sleeping through it");
	let span = span_of_first(&pulls, WANT).expect("the consumer retried at least seven times");
	assert!(
		span >= Duration::from_millis(1500),
		"a failing cycle must back off, not retry at the poll interval — {WANT} attempts inside {span:?} at a {POLL:?} poll"
	);
}

/// A CYCLE THAT CANNOT MOVE THE CURSOR MUST SLOW DOWN TOO, EVEN THOUGH IT RETURNS CLEANLY.
///
/// The two states this bridge can get stuck in — an unreadable kind, and a `next_position`
/// that did not advance — both end the drain with `Ok(())`, because neither is an error:
/// the batch WAS applied, it just cannot be consumed. Nothing about that resolves on its
/// own; it takes a deploy or a server-side fix. So without this, the bridge re-pulls and
/// re-applies the identical batch every poll for as long as it takes someone to notice —
/// indefinite load on both planes, at the production interval — while looking, from the
/// outside, exactly like a healthy cycle (#181).
///
/// The bound is a lower one, like its failing-cycle neighbour, and for the same reason: the
/// only thing that can stretch seven pulls this far apart is the delay doubling.
#[tokio::test]
async fn a_cycle_that_cannot_move_the_cursor_backs_off_although_it_returned_ok() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = unique_subject();
	// A row already past the event's sequence, so every apply is the cheap redelivery no-op
	// and what paces the pulls is the wait, not the work.
	PgUsers::new(pool.clone())
		.provision(
			domain::auth::AuthSubject::parse(&subject).unwrap(),
			domain::users::Email::parse("wedged@example.com").unwrap(),
			true,
		)
		.await
		.expect("provision the subject the wedged batch names");
	sqlx::query("UPDATE users SET last_lifecycle_sequence = 9 WHERE auth_subject = $1")
		.bind(&subject)
		.execute(&pool)
		.await
		.unwrap();

	// One event per pull, so `drain` returns on the short batch and the cycle ends in
	// `Ok(())` — the arm under test. Seven cycles at a 50 ms poll: 0.3 s if the wedge keeps
	// the poll interval, 3.1 s once the wait doubles each time (50 + 100 + … + 1600).
	const POLL: Duration = Duration::from_millis(50);
	const WANT: usize = 7;
	let pulls = pull_log();
	let finished = drive_misbehaving(
		&pool,
		StuckUserEvents {
			event: event(&subject, Kind::SessionsRevoked, 1),
			full: false,
			pulls: pulls.clone(),
		},
		POLL,
		pulls.clone(),
		WANT,
	)
	.await;

	assert!(finished, "a backing-off consumer still winds down on cancellation rather than sleeping through it");
	let span = span_of_first(&pulls, WANT).expect("the consumer pulled at least seven times");
	assert!(
		span >= Duration::from_millis(1500),
		"a cycle that could not move the cursor must back off, not re-pull at the poll interval — {WANT} pulls inside {span:?} at a {POLL:?} poll"
	);
	assert_eq!(cursor_position(&pool).await, 0, "the premise of the test: not one of those cycles consumed anything");
}

/// A BATCH THE CURSOR CANNOT MOVE PAST IS STILL APPLIED.
///
/// Ending the drain protects the identity plane from a hot loop. It must not also stop the
/// freeze from reaching the money gate: the cursor standing still means concierge keeps the
/// batch and re-delivers it — to this build too, every poll — and applying it is the same
/// idempotent no-op a redelivery already is, so holding it back buys nothing and leaves a
/// suspension, a tier downgrade or a revoke floor unmirrored until a build that can move
/// past the batch ships. That is strictly worse than the hot loop this replaced, which at
/// least mirrored (#181).
///
/// One event per pull, so `drain` stops on the short batch whatever the cursor does: what
/// is under test here is the apply, not the looping.
#[tokio::test]
async fn a_batch_the_cursor_cannot_move_past_is_still_applied() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = unique_subject();
	PgUsers::new(pool.clone())
		.provision(
			domain::auth::AuthSubject::parse(&subject).unwrap(),
			domain::users::Email::parse("stuck-apply@example.com").unwrap(),
			true,
		)
		.await
		.expect("provision the subject the stuck batch suspends");

	const POLL: Duration = Duration::from_millis(50);
	// Two pulls: the consumer is sequential, so the second one having been recorded means
	// the first one's batch was applied to completion, not merely received.
	const WANT: usize = 2;
	let pulls = pull_log();
	let finished = drive_misbehaving(
		&pool,
		StuckUserEvents {
			event: event(&subject, Kind::Suspended, 1),
			full: false,
			pulls: pulls.clone(),
		},
		POLL,
		pulls.clone(),
		WANT,
	)
	.await;

	assert!(finished, "the consumer winds down on cancellation");
	let user_id = user_id_for(&pool, &subject).await.expect("the subject keeps its banking row");
	assert!(
		blocked(&pool, user_id).await,
		"a SUSPENDED the cursor cannot move past must still freeze the user — refusing to apply it leaves the money gate open on rules concierge has already withdrawn"
	);
	assert_eq!(
		cursor_position(&pool).await,
		0,
		"and nothing is consumed: the batch stays in concierge's outbox for a build that can move past it"
	);
}

/// A CYCLE THAT MIRRORED A BATCH MUST NOT BACK OFF.
///
/// `drain` pulls repeatedly, so a backlog larger than one batch can fail on its Nth pull
/// with N-1 batches already applied and committed. Treating that as a failed cycle walks the
/// retry delay up to the one-minute ceiling while the bridge is in fact draining — minutes
/// added to how long a freeze takes to reach the money gate, against a concierge that is
/// merely rolling rather than down (#181).
///
/// The bound here is an upper one, unlike its neighbours: seventeen pulls is nine cycles,
/// which the backing-off consumer cannot reach inside [`OBSERVE_CAP`] because it sleeps
/// 0.1 + 0.2 + … + 12.8 s ≈ 25 s first, while a consumer that keeps the poll interval needs
/// nine polls and nine batches of no-op applies — a couple of seconds.
#[tokio::test]
async fn a_cycle_that_mirrored_a_batch_does_not_back_off() {
	let Some(pool) = pool().await else {
		eprintln!("DATABASE_URL unset — skipping real-DB test");
		return;
	};
	let subject = unique_subject();
	// A row already past the event's sequence: the batches exist to move the CURSOR, and
	// every apply is meant to be the cheap redelivery no-op, not a mutation.
	PgUsers::new(pool.clone())
		.provision(
			domain::auth::AuthSubject::parse(&subject).unwrap(),
			domain::users::Email::parse("flaky@example.com").unwrap(),
			true,
		)
		.await
		.expect("provision the subject the flaky batches name");
	sqlx::query("UPDATE users SET last_lifecycle_sequence = 9 WHERE auth_subject = $1")
		.bind(&subject)
		.execute(&pool)
		.await
		.unwrap();

	const POLL: Duration = Duration::from_millis(100);
	const WANT: usize = 17;
	let pulls = pull_log();
	let finished = drive_misbehaving(
		&pool,
		FlakyUserEvents {
			event: event(&subject, Kind::SessionsRevoked, 1),
			pulls: pulls.clone(),
		},
		POLL,
		pulls.clone(),
		WANT,
	)
	.await;

	assert!(finished, "the consumer winds down on cancellation");
	assert!(
		span_of_first(&pulls, WANT).is_some(),
		"a cycle that mirrored a batch must retry at the poll interval, not back off — fewer than {WANT} pulls arrived within {OBSERVE_CAP:?} at a {POLL:?} poll"
	);
	assert!(cursor_position(&pool).await > 0, "the premise of the test: those cycles really did commit batches before failing");
}
