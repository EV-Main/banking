//! Integration tests for the money plane — real Postgres **and** TigerBeetle (no
//! mocks, per the project rules). They run when `DATABASE_URL` is set and a
//! TigerBeetle replica is reachable (e.g. `nix run .#db` + `.#tb`), and skip
//! otherwise so a DB-less `cargo test` still passes. Each test uses fresh random
//! ids (user/service), so runs are isolated on shared infrastructure. The relay is
//! driven explicitly to apply committed events deterministically: every test holds
//! `common::outbox_serial()` for its whole life (the suite's tests share one database,
//! and two relays draining it at once pick up the same row) and drains through
//! `common::drain_to_quiescence`, because one `drain` pass returns early on a transient
//! failure and leaves the caller reading a ledger its own rows have not reached (#294,
//! #437). The three tests whose expected outcome IS a park keep the one-pass `drain`.
//!
//! Boundary authz (`require_permission`/`caller_id`) is the same path the live `UsersSvc`
//! already uses; the load-bearing money invariant — the revoke rule — is exercised
//! here at the aggregate/repository layer.

use std::sync::Arc;

use domain::{
	allocations::AllocationAccess,
	auth::AuthSubject,
	balance::{LedgerAccountKey, ServiceId, TransferCode, ValuationId},
	error::DomainError,
	money::{Nav, Network, Shares, TxRef, Usdt, WalletAddress},
	redemptions::RedemptionState,
	subscriptions::{Subscription, SubscriptionId},
	users::{Email, UserId},
	withdrawals::WithdrawalId,
};
use piggybank_core::{
	application::{balance as balance_app, funds as funds_app, withdrawals as withdrawal_app},
	config::KycGate,
	infrastructure::{
		allocations::PgAllocations, custody::StubCustody, deposits::PgDeposits, nav::PgNav, outflow::PgOutflowPolicy, positions::PgFundPositions, redemptions::PgRedemptions, relay::Relay,
		subscriptions::PgSubscriptions, users::PgUsers, withdrawals::PgWithdrawals,
	},
	ports::{
		AllocationRegistry, FundPositionReader, RedemptionRepository, SubscriptionRepository, UserRepository,
		ledger::{Ledger, LedgerError, LedgerTransfer},
		nav::NavMarks,
	},
};
use sqlx::PgPool;
use tokio::sync::{MutexGuard, Notify};
use uuid::Uuid;

mod common;

struct Harness {
	pool: PgPool,
	deposits: PgDeposits,
	allocations: PgAllocations,
	ledger: Arc<dyn Ledger>,
	relay: Relay,
	notify: Arc<Notify>,
	/// Held for the test's whole life — declared last so it is released after the relay.
	_serial: MutexGuard<'static, ()>,
}

async fn harness() -> Option<Harness> {
	let serial = common::outbox_serial().await;
	let pool = common::pool().await?;
	let ledger = common::seeded_ledger(&pool, "money-plane test").await?;

	let notify = Arc::new(Notify::new());
	let relay = Relay::new(pool.clone(), ledger.clone(), Arc::new(StubCustody), notify.clone());
	let deposits = PgDeposits::new(pool.clone());
	let allocations = PgAllocations::new(pool.clone());
	Some(Harness {
		pool,
		deposits,
		allocations,
		ledger,
		relay,
		notify,
		_serial: serial,
	})
}

fn usdt(decimal: &str) -> Usdt {
	Usdt::parse_decimal(decimal).unwrap()
}

fn unique_service() -> ServiceId {
	ServiceId::parse(&format!("svc-{}", Uuid::new_v4())).unwrap()
}

/// A unique service that is also REGISTERED, OPEN and at `invest` access in the
/// allocation registry — the state every money path now requires of a product that
/// deals with anyone. Tests that only poke the ledger directly keep using the bare
/// [`unique_service`]; the access tests below register their own at the locked default.
async fn registered_service(h: &Harness) -> ServiceId {
	let service = registered_locked_service(h).await;
	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	service
}

/// Registered and open, but left at the access level a registration lands on (`view`):
/// listed, and refusing money from anyone an operator has not let in.
async fn registered_locked_service(h: &Harness) -> ServiceId {
	let service = unique_service();
	let mut allocation = domain::allocations::Allocation::register(
		domain::allocations::AllocationId::new(),
		service.clone(),
		"itest fund",
		"",
		domain::allocations::AllocationIcon::default(),
	)
	.unwrap();
	h.allocations.register(&mut allocation).await.unwrap();
	h.allocations.open(&service).await.unwrap();
	service
}

fn unique_tx_ref() -> TxRef {
	TxRef::parse(&format!("itest-{}", Uuid::new_v4())).unwrap()
}

/// A product's off-platform return landing on its claim: custody grows and the
/// allocation's claim with it. Posted straight to the ledger because no deposit may credit
/// an allocation (#245, `deposits.party_kind = 'user'`) — this is the test standing in for
/// the fund's realised profit, not a path the hub offers.
async fn realize_profit(h: &Harness, service: &ServiceId, amount: Usdt) {
	let transfer = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::CryptoWallet(Network::Bep20),
		credit: LedgerAccountKey::ServiceClaim(service.clone()),
		amount: amount.base_units(),
		code: TransferCode::Deposit,
		reference: 0,
	};
	h.ledger.post(&transfer).await.unwrap();
}

async fn claim(h: &Harness, key: &LedgerAccountKey) -> Usdt {
	Usdt::from_base_units(h.ledger.balance(key).await.unwrap().posted)
}

fn shares(decimal: &str) -> Shares {
	Shares::parse_decimal(decimal).unwrap()
}

async fn units(h: &Harness, key: &LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(key).await.unwrap().posted)
}

async fn units_available(h: &Harness, key: &LedgerAccountKey) -> Shares {
	Shares::from_base_units(h.ledger.balance(key).await.unwrap().available())
}

fn now_unix() -> i64 {
	use std::time::{SystemTime, UNIX_EPOCH};
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Provision a fresh active, KYC-verified user — the withdrawal path's Read-First requires
/// one, and a real row keeps the cross-flow tests faithful to production. `provision` leaves
/// the row at tier 0 (the schema default), which the withdrawal gate refuses, so the mirrored
/// tier is set the way the lifecycle bridge sets it.
async fn active_user(pool: &PgPool, users: &PgUsers) -> UserId {
	let subject = AuthSubject::parse(&format!("itest-{}", Uuid::new_v4())).unwrap();
	let email = Email::parse(&format!("u{}@example.com", Uuid::new_v4().simple())).unwrap();
	let user = users.provision(subject, email, true).await.unwrap().id();
	common::set_kyc_level(pool, user, 1).await;
	user
}

/// The user's cost-basis projection for a fund (None when no `fund_positions` row exists).
async fn cost_basis(positions: &PgFundPositions, user: UserId, service: &ServiceId) -> Option<Usdt> {
	positions.find(user, service).await.unwrap().map(|p| p.cost_basis)
}

/// The projection-tracked remaining units for a fund — the cost-basis reduction's
/// denominator, read straight from the column (the read port does not surface it).
async fn tracked_units(pool: &PgPool, user: UserId, service: &ServiceId) -> Option<Shares> {
	let raw: Option<String> = sqlx::query_scalar("SELECT units FROM fund_positions WHERE user_id = $1 AND service = $2")
		.bind(user.raw())
		.bind(service.as_str())
		.fetch_optional(pool)
		.await
		.unwrap();
	raw.map(|s| Shares::from_base_units(s.parse().unwrap()))
}

/// A structurally valid destination address for a withdrawal (distinct from the user's own).
fn destination(network: Network) -> WalletAddress {
	let raw = match network {
		Network::Bep20 | Network::Polygon => "0x52908400098527886E0F7030069857D2E4169EE7",
		Network::Trc20 => "TJRabPrwbZy45sbavfcjinPJC18kjpRTv8",
		Network::Ton => "EQCD39VS5jcptHL8vMjEXrzGaRcCVYto7HUn4bpAOg8xqB2N",
	};
	WalletAddress::parse(network, raw).unwrap()
}

#[tokio::test]
async fn deposit_credits_once_and_is_idempotent_by_tx_ref() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let network = Network::Trc20;
	let key = LedgerAccountKey::UserClaim(user);
	let tx_ref = unique_tx_ref();

	assert!(claim(&h, &key).await.is_zero());

	let recorded = balance_app::record_deposit(&h.deposits, &h.notify, tx_ref.clone(), user, network, usdt("100")).await.unwrap();
	assert!(recorded, "first record is new");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(claim(&h, &key).await, usdt("100"), "the deposit credited the user's claim");

	// Re-recording the same chain tx is a no-op — no second event, no double credit.
	let again = balance_app::record_deposit(&h.deposits, &h.notify, tx_ref, user, network, usdt("100")).await.unwrap();
	assert!(!again, "duplicate tx_ref is idempotent");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(claim(&h, &key).await, usdt("100"), "no double credit on a duplicate");
}

#[tokio::test]
async fn deposit_credits_a_claim_backed_by_custody() {
	let Some(h) = harness().await else { return };
	let network = Network::Ton;
	let wallet = LedgerAccountKey::CryptoWallet(network);
	let user = UserId::new();

	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt("250")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// The user's unified claim is isolated (random user); the rail's custody wallet is a
	// shared singleton. Assert the direction that holds regardless of concurrent
	// deposits: the rail that funded this claim backs it (global sum custody >= claims).
	let user_claim = claim(&h, &LedgerAccountKey::UserClaim(user)).await;
	assert_eq!(user_claim, usdt("250"), "the deposit credited the user's claim");
	assert!(claim(&h, &wallet).await >= user_claim, "custody backs the claim (sum custody >= claims)");
}

#[tokio::test]
async fn non_negative_flag_is_the_ledger_backstop() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let network = Network::Bep20;
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt("10")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Bypass the application check and over-debit the claim directly: TB's
	// DebitsMustNotExceedCredits flag rejects it as InsufficientFunds.
	let transfer = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::UserClaim(user),
		credit: LedgerAccountKey::ServiceClaim(unique_service()),
		amount: usdt("50").base_units(),
		code: TransferCode::UserAllocate,
		reference: 0,
	};
	let err = h.ledger.post(&transfer).await.unwrap_err();
	assert!(matches!(err, LedgerError::InsufficientFunds), "the flag rejected the over-debit, got {err:?}");
}

#[tokio::test]
async fn transfer_id_is_idempotent_no_double_move() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let network = Network::Ton;
	let service = unique_service();
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt("100")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let transfer = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::UserClaim(user),
		credit: LedgerAccountKey::ServiceClaim(service.clone()),
		amount: usdt("30").base_units(),
		code: TransferCode::UserAllocate,
		reference: 0,
	};
	// Same deterministic id twice ⇒ the second is `Exists` ⇒ applied exactly once.
	h.ledger.post(&transfer).await.unwrap();
	h.ledger.post(&transfer).await.unwrap();

	assert_eq!(claim(&h, &LedgerAccountKey::ServiceClaim(service)).await, usdt("30"), "no double move");
	assert_eq!(claim(&h, &LedgerAccountKey::UserClaim(user)).await, usdt("70"));
}

#[tokio::test]
async fn share_ledger_mints_burns_and_rejects_over_redeem() {
	let Some(h) = harness().await else { return };
	let user = UserId::new();
	let service = unique_service();
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let outstanding = LedgerAccountKey::SharesOutstanding(service.clone());

	// Mint 100 units on the Share ledger: Dr UserShares / Cr SharesOutstanding.
	let mint = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: user_shares.clone(),
		credit: outstanding.clone(),
		amount: shares("100").base_units(),
		code: TransferCode::ShareMint,
		reference: 0,
	};
	h.ledger.post(&mint).await.unwrap();
	// Per-service invariant: SharesOutstanding == sum(UserShares). This (user, service) is
	// fresh, so both equal the minted amount exactly.
	assert_eq!(units(&h, &user_shares).await, shares("100"), "the user holds the minted units");
	assert_eq!(units(&h, &outstanding).await, shares("100"), "supply equals the user's holding");

	// Over-redeem: a *pending* burn of 150 (Dr SharesOutstanding / Cr UserShares) exceeds
	// the 100 minted — TigerBeetle rejects it atomically via the non-negative flag, even as
	// a reservation. The TB flag, not a row-lock, is the money backstop.
	let over_burn = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: outstanding.clone(),
		credit: user_shares.clone(),
		amount: shares("150").base_units(),
		code: TransferCode::ShareBurn,
		reference: 0,
	};
	let err = h.ledger.reserve(&over_burn).await.unwrap_err();
	assert!(matches!(err, LedgerError::InsufficientFunds), "TB rejects the over-redeem reserve, got {err:?}");

	// A burn within the holding posts and reduces both sides equally — the invariant holds.
	let burn = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: outstanding.clone(),
		credit: user_shares.clone(),
		amount: shares("40").base_units(),
		code: TransferCode::ShareBurn,
		reference: 0,
	};
	h.ledger.post(&burn).await.unwrap();
	assert_eq!(units(&h, &user_shares).await, shares("60"), "the user's units dropped by the burn");
	assert_eq!(units(&h, &outstanding).await, shares("60"), "supply dropped in lockstep");
}

#[tokio::test]
async fn fund_valuation_derives_nav_and_guards_fat_finger() {
	let Some(h) = harness().await else { return };
	let nav_repo = PgNav::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;

	// No mark yet → the current NAV is the bootstrap seed (1.0).
	assert_eq!(nav_repo.current(&service).await.unwrap().map(|v| v.nav).unwrap_or(Nav::SEED), Nav::SEED);
	// Posting AUM with zero units outstanding is rejected — NAV is undefined.
	assert!(
		funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("100"), "op", now_unix())
			.await
			.is_err()
	);

	// Mint 100 units so SharesOutstanding(service) = 100, then NAV can be derived.
	let mint = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::UserShares(service.clone(), user),
		credit: LedgerAccountKey::SharesOutstanding(service.clone()),
		amount: shares("100").base_units(),
		code: TransferCode::ShareMint,
		reference: 0,
	};
	h.ledger.post(&mint).await.unwrap();

	// AUM 150 over 100 units → NAV 1.5, and it becomes the current price.
	let v = funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("150"), "op", now_unix())
		.await
		.unwrap();
	assert_eq!(v.nav, Nav::parse_decimal("1.5").unwrap());
	assert_eq!(nav_repo.current(&service).await.unwrap().map(|v| v.nav).unwrap_or(Nav::SEED), Nav::parse_decimal("1.5").unwrap());

	// A 10x fat-finger (AUM 1500 → NAV 15, +900%) is rejected, and the refusal names the
	// only way past the guard. There is no flag on this path that lifts it (banking#232).
	let err = funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("1500"), "op", now_unix())
		.await
		.unwrap_err();
	assert!(matches!(&err, DomainError::Validation(reason) if reason.contains("valuation-override consilium")), "got {err:?}");
	assert_eq!(
		nav_repo.current(&service).await.unwrap().unwrap().nav,
		Nav::parse_decimal("1.5").unwrap(),
		"the refused mark was not recorded"
	);
}

/// The cap is measured against the rolling window's anchor as well as the previous mark,
/// so it cannot be walked past in steps that are each inside it.
#[tokio::test]
async fn the_nav_move_cap_cannot_be_stepped_past_inside_the_window() {
	let Some(h) = harness().await else { return };
	let nav_repo = PgNav::new(h.pool.clone());
	let service = registered_service(&h).await;
	let now = now_unix();
	h.ledger
		.post(&LedgerTransfer {
			id: Uuid::new_v4().as_u128(),
			debit: LedgerAccountKey::UserShares(service.clone(), UserId::new()),
			credit: LedgerAccountKey::SharesOutstanding(service.clone()),
			amount: shares("100").base_units(),
			code: TransferCode::ShareMint,
			reference: 0,
		})
		.await
		.unwrap();
	let post = |aum: &'static str, at: i64| funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt(aum), "op", at);

	// The first mark anchors the window: NAV 1.0.
	post("100", now).await.unwrap();
	// +45% against both the previous mark and the anchor: fine.
	assert_eq!(post("145", now).await.unwrap().nav, Nav::parse_decimal("1.45").unwrap());
	// Another +45% is inside the cap against the previous mark (1.45 → 2.10, +44.8%) and
	// was the whole of the stepping attack; against the anchor it is +110%, and refused.
	let err = post("210", now).await.unwrap_err();
	assert!(matches!(&err, DomainError::Validation(reason) if reason.contains("anchoring the rolling window")), "got {err:?}");
	// A small move that stays inside the cap against BOTH still lands: 1.48 is +2% on the
	// previous mark and +48% on the anchor.
	assert_eq!(post("148", now).await.unwrap().nav, Nav::parse_decimal("1.48").unwrap());
	assert_eq!(nav_repo.current(&service).await.unwrap().unwrap().nav, Nav::parse_decimal("1.48").unwrap());

	// The window slides. Once every mark so far is older than the window, the anchor is
	// the newest of them (1.48), and the same +42% that was refused above now passes.
	sqlx::query("UPDATE fund_valuations SET posted_at = posted_at - interval '8 days' WHERE service = $1")
		.bind(service.as_str())
		.execute(&h.pool)
		.await
		.unwrap();
	let anchor = nav_repo.anchor(&service, now - funds_app::NAV_MOVE_WINDOW_SECS).await.unwrap().unwrap();
	assert_eq!(anchor.nav, Nav::parse_decimal("1.48").unwrap(), "the newest mark outside the window anchors it");
	assert_eq!(post("210", now).await.unwrap().nav, Nav::parse_decimal("2.1").unwrap());
	// And a fund younger than the window is anchored by its first mark, never by nothing:
	// the earliest mark is the fallback, so a fresh fund cannot be stepped either.
	let young = registered_service(&h).await;
	assert!(nav_repo.anchor(&young, now).await.unwrap().is_none(), "never marked: no anchor and no previous mark either");
}

#[tokio::test]
async fn subscribe_mints_units_moves_cash_and_prices_at_nav() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();
	let user_claim = LedgerAccountKey::UserClaim(user);
	let service_claim = LedgerAccountKey::ServiceClaim(service.clone());
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let outstanding = LedgerAccountKey::SharesOutstanding(service.clone());

	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("400"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// First subscription mints at the seed NAV (1.0): 200 cash → 200 units.
	funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("200"), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(claim(&h, &user_claim).await, usdt("200"), "cash left the user's claim");
	assert_eq!(claim(&h, &service_claim).await, usdt("200"), "cash entered the fund pool");
	assert_eq!(units(&h, &user_shares).await, shares("200"), "units minted to the user");
	assert_eq!(units(&h, &outstanding).await, shares("200"), "supply grew with the mint");

	// Operator marks the fund up to NAV 2.0 (AUM 400 over 200 units).
	funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("400"), "op", now_unix())
		.await
		.unwrap();

	// A second subscription prices at NAV 2.0: 100 cash → 50 units (fractional pricing).
	funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("100"), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units(&h, &user_shares).await, shares("250"), "50 more units at NAV 2");
	assert_eq!(claim(&h, &user_claim).await, usdt("100"), "cash dropped by the second subscription");

	// Dealing on a stale mark (now far past the last valuation) is refused — the user has
	// the balance, so this isolates the staleness guard.
	let stale_now = now + funds_app::MAX_NAV_AGE_SECS + 100;
	assert!(
		funds_app::subscribe(
			&funds_app::FundPorts {
				allocations: &h.allocations,
				ledger: h.ledger.as_ref(),
				nav: &nav_repo,
				relay: &h.notify
			},
			&subs,
			user,
			service.clone(),
			usdt("50"),
			stale_now
		)
		.await
		.is_err()
	);

	// Subscribing beyond the available balance is rejected Read-First.
	assert!(
		funds_app::subscribe(
			&funds_app::FundPorts {
				allocations: &h.allocations,
				ledger: h.ledger.as_ref(),
				nav: &nav_repo,
				relay: &h.notify
			},
			&subs,
			user,
			service.clone(),
			usdt("1000"),
			now
		)
		.await
		.is_err()
	);
}

/// The performance chart's two series: the marks in the window, oldest first, and the
/// holder's participation walked back from the live holding — with the window's start
/// priced off the mark BEFORE it and a past `to` valued as of then, not as of now.
#[tokio::test]
async fn fund_nav_history_lists_the_window_and_values_the_holder_through_it() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let positions = PgFundPositions::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let user = UserId::new();
	let service = registered_service(&h).await;
	let history = |from: i64, to: i64, now: i64| funds_app::fund_nav_history(&h.allocations, &nav_repo, h.ledger.as_ref(), &positions, service.clone(), user, false, from, to, now);

	// Never marked, never held: no marks and no participation — and not an error.
	let blank = history(0, 0, now_unix()).await.unwrap();
	assert!(
		blank.marks.is_empty() && blank.participation.is_empty() && !blank.truncated,
		"seed-only fund answers empty series"
	);

	// Day −4: 200 cash → 200 units at the seed NAV. Day −3: marked to 1.5. Now: marked to 2.
	let day = 24 * 60 * 60;
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("400"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("200"), now_unix()).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let sub_at = now_unix() - 4 * day;
	sqlx::query("UPDATE subscriptions SET created_at = to_timestamp($2) WHERE user_id = $1")
		.bind(user.raw())
		.bind(sub_at as f64)
		.execute(&h.pool)
		.await
		.unwrap();
	funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("300"), "op", now_unix())
		.await
		.unwrap();
	let mark1_at = now_unix() - 3 * day;
	sqlx::query("UPDATE fund_valuations SET posted_at = to_timestamp($3) WHERE service = $1 AND nav = $2")
		.bind(service.as_str())
		.bind(Nav::parse_decimal("1.5").unwrap().base_units().to_string())
		.bind(mark1_at as f64)
		.execute(&h.pool)
		.await
		.unwrap();
	let mark2 = funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("400"), "op", now_unix())
		.await
		.unwrap();
	let now = now_unix();
	assert!(mark2.posted_at_unix <= now);

	// All-time: both marks, oldest first; the holder's line starts at their subscription.
	let all = history(0, 0, now).await.unwrap();
	assert_eq!(
		all.marks.iter().map(|m| (m.posted_at_unix, m.nav)).collect::<Vec<_>>(),
		vec![(mark1_at, Nav::parse_decimal("1.5").unwrap()), (mark2.posted_at_unix, mark2.nav)]
	);
	assert_eq!(all.marks[1].aum, usdt("400"));
	assert!(!all.truncated);
	let series: Vec<(i64, Usdt)> = all.participation.iter().map(|p| (p.at_unix, p.value)).collect();
	assert_eq!(series[0], (sub_at, usdt("200")), "200 units at the seed NAV when they arrived");
	assert_eq!(series[1], (mark1_at, usdt("300")), "revalued at the first mark");
	assert_eq!(series.last().copied(), Some((now, usdt("400"))), "ends now, at the live holding × the current mark");
	assert!(series.iter().filter(|(at, _)| *at >= mark2.posted_at_unix).all(|(_, v)| *v == usdt("400")));

	// A window opening on day −2: only the second mark, and the line opens at `from`
	// priced off the mark before the window (1.5), not the seed.
	let recent = history(now - 2 * day, 0, now).await.unwrap();
	assert_eq!(recent.marks.len(), 1);
	assert_eq!(recent.marks[0].nav, mark2.nav);
	assert_eq!(recent.participation.first().map(|p| (p.at_unix, p.value)), Some((now - 2 * day, usdt("300"))));
	assert_eq!(recent.participation.last().map(|p| p.value), Some(usdt("400")));

	// A window that ends yesterday: the first mark only, and the last point is AS OF then.
	let past = history(0, now - day, now).await.unwrap();
	assert_eq!(past.marks.iter().map(|m| m.nav).collect::<Vec<_>>(), vec![Nav::parse_decimal("1.5").unwrap()]);
	assert_eq!(past.participation.last().map(|p| (p.at_unix, p.value)), Some((now - day, usdt("300"))));

	// An inverted window is a client error, not an empty answer.
	assert!(matches!(history(now, now - day, now).await.unwrap_err(), DomainError::Validation(_)));

	// A stranger sees the marks but has no line.
	let stranger = funds_app::fund_nav_history(&h.allocations, &nav_repo, h.ledger.as_ref(), &positions, service.clone(), UserId::new(), false, 0, 0, now)
		.await
		.unwrap();
	assert_eq!(stranger.marks.len(), 2);
	assert!(stranger.participation.is_empty());
}

#[tokio::test]
async fn redeem_when_fund_is_liquid_auto_completes() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();
	let user_claim = LedgerAccountKey::UserClaim(user);
	let service_claim = LedgerAccountKey::ServiceClaim(service.clone());
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);
	let outstanding = LedgerAccountKey::SharesOutstanding(service.clone());

	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("100"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("100"), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Redeem 40 units at the seed NAV (1.0) → 40 cash. The fund claim (100) covers it, so
	// it auto-settles in one request (a separate settle command, not a co-emitted event).
	let r = funds_app::request_redemption(&fund_ports, &reds, user, service.clone(), shares("40"), now).await.unwrap();
	assert_eq!(r.state(), RedemptionState::Completed, "a liquid redemption settles immediately");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(units(&h, &user_shares).await, shares("60"), "units burned");
	assert_eq!(units(&h, &outstanding).await, shares("60"), "supply dropped with the burn");
	assert_eq!(claim(&h, &service_claim).await, usdt("60"), "cash left the fund pool");
	assert_eq!(claim(&h, &user_claim).await, usdt("40"), "cash credited to the user");
}

/// Set a user up holding 100 units (subscribed with 100 cash at the seed NAV, so basis 100)
/// in a fund marked up to `aum` — pick it so redeeming `units` prices above the fund's 100
/// claim — then request that redemption, returning its queued id. Shared by the short-fund
/// tests.
#[allow(clippy::too_many_arguments)]
async fn queued_short_redemption(
	h: &Harness,
	subs: &PgSubscriptions,
	reds: &PgRedemptions,
	nav_repo: &PgNav,
	user: UserId,
	service: &ServiceId,
	now: i64,
	aum: &str,
	units: &str,
) -> domain::redemptions::RedemptionId {
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: nav_repo,
		relay: &h.notify,
	};
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("100"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::subscribe(&fund_ports, subs, user, service.clone(), usdt("100"), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	// Any AUM over the 100-unit supply moves the NAV past the guard here, and the guarded
	// post has no flag to lift it any more — the mark is written through the shared writer
	// the owners' override uses, which is exactly what an executed override would do.
	funds_app::record_valuation(nav_repo, h.ledger.as_ref(), ValuationId::new(), service.clone(), usdt(aum), "op")
		.await
		.unwrap();
	let r = funds_app::request_redemption(&fund_ports, reds, user, service.clone(), shares(units), now).await.unwrap();
	assert_eq!(r.state(), RedemptionState::Queued, "a short fund queues the redemption");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	r.id()
}

#[tokio::test]
async fn redeem_on_a_short_fund_queues_then_settles_with_profit() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();
	let user_claim = LedgerAccountKey::UserClaim(user);
	let service_claim = LedgerAccountKey::ServiceClaim(service.clone());
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let id = queued_short_redemption(&h, &subs, &reds, &nav_repo, user, &service, now, "200", "100").await;
	// Units are reserved (locked), not burned; no cash paid while queued.
	assert_eq!(units(&h, &user_shares).await, shares("100"), "units still posted (reserved, not burned)");
	assert_eq!(units_available(&h, &user_shares).await, Shares::ZERO, "units locked by the reservation");
	assert_eq!(claim(&h, &user_claim).await, Usdt::ZERO, "no cash paid while queued");

	// The fund realizes profit: a deposit credits the service claim up to 200.
	realize_profit(&h, &service, usdt("100")).await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(claim(&h, &service_claim).await, usdt("200"), "the fund topped up");

	// Operator settles — priced at the settle-time NAV (2) and paid in full.
	let settled = funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, id, now).await.unwrap();
	assert_eq!(settled.state(), RedemptionState::Completed);
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(units(&h, &user_shares).await, Shares::ZERO, "all units burned at settle");
	assert_eq!(claim(&h, &service_claim).await, Usdt::ZERO, "the fund paid out");
	assert_eq!(claim(&h, &user_claim).await, usdt("200"), "100 in, 200 out — NAV doubled, profit realized");
}

#[tokio::test]
async fn settling_a_short_fund_parks_without_burning_or_paying() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();
	let user_claim = LedgerAccountKey::UserClaim(user);
	let service_claim = LedgerAccountKey::ServiceClaim(service.clone());
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let id = queued_short_redemption(&h, &subs, &reds, &nav_repo, user, &service, now, "200", "100").await;

	// Settle while the fund is STILL short (100 < 200) — the relay's payout pre-check parks
	// the whole event. Burn-first ordering means nothing is applied: no half-burn, no cash.
	funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, id, now).await.unwrap();
	// The park IS what this test pins, so the one-pass drain stays here: `drain_to_quiescence`
	// reports a park raised during the call as a failure.
	h.relay.drain().await;
	assert_eq!(units(&h, &user_shares).await, shares("100"), "units NOT burned (settle parked)");
	assert_eq!(claim(&h, &user_claim).await, Usdt::ZERO, "no cash paid (settle parked)");
	assert_eq!(claim(&h, &service_claim).await, usdt("100"), "the fund claim is untouched");
}

#[tokio::test]
async fn cancelling_a_queued_redemption_returns_the_units() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();
	let user_shares = LedgerAccountKey::UserShares(service.clone(), user);

	let id = queued_short_redemption(&h, &subs, &reds, &nav_repo, user, &service, now, "200", "100").await;

	// A non-owner cannot cancel.
	assert!(funds_app::cancel_redemption(&reds, &h.notify, id, UserId::new()).await.is_err(), "only the owner may cancel");

	// The owner cancels — the relay voids the burn, releasing the locked units.
	let cancelled = funds_app::cancel_redemption(&reds, &h.notify, id, user).await.unwrap();
	assert_eq!(cancelled.state(), RedemptionState::Cancelled);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units(&h, &user_shares).await, shares("100"), "units still held");
	assert_eq!(units_available(&h, &user_shares).await, shares("100"), "the reservation lock was released");
}

// BANK-MONEY-1: the cost-basis projection is now written by the relay AFTER the cash leg
// posts, never on the synchronous `open` path. So a subscription whose cash leg parks
// (insufficient claim) leaves NO `fund_positions` row — no phantom basis without units/cash.
// The `open` repo is called directly to bypass the application Read-First and force the relay
// to face an over-subscription, the exact race the fix guards.
#[tokio::test]
async fn a_parked_subscribe_cash_leg_leaves_no_cost_basis() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let positions = PgFundPositions::new(h.pool.clone());
	let user = UserId::new();
	let service = unique_service();
	let user_claim = LedgerAccountKey::UserClaim(user);

	// Fund only 50, then commit a 100-cash subscription straight through the repo (skipping the
	// application solvency check) so the relay's cash leg `Dr user / Cr service` overdraws.
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("50"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let mut subscription = Subscription::open(SubscriptionId::new(), user, service.clone(), usdt("100"), Nav::SEED).unwrap();
	subs.open(&mut subscription, &piggybank_core::ports::ledger::NoSpendCheck).await.unwrap();
	// The park IS what this test pins, so the one-pass drain stays here: `drain_to_quiescence`
	// reports a park raised during the call as a failure.
	h.relay.drain().await;

	// TB's flag parked the cash leg: no cash moved, no units minted, and — the fix — no basis.
	assert_eq!(claim(&h, &user_claim).await, usdt("50"), "the cash never left the claim (cash leg parked)");
	assert_eq!(units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await, Shares::ZERO, "no units minted");
	assert!(cost_basis(&positions, user, &service).await.is_none(), "a parked subscribe must leave no phantom cost_basis");
}

// BANK-ARCH-02: a concurrent withdraw + subscribe on the same UserClaim must serialize on the
// shared per-user lock so the system never ends in a silently-divergent state. With 100
// deposited, an 80-gross withdrawal and an 80-cash subscription cannot both apply (160 > 100):
// the shared lock orders the two `open` commits, and the relay (single-worker, seq-ordered)
// then applies the first reservation while the second hits TB's non-negative flag and PARKS —
// a recoverable terminal, not a silent over-commit. The combined fix guarantees the only
// observable end state is consistent: exactly one 80-spend posts, the claim never goes
// negative, and — the BANK-MONEY-1 half — the subscribe leaves cost_basis ONLY when its cash
// leg actually posted (never a phantom basis behind a parked cash leg).
#[tokio::test]
async fn concurrent_withdraw_and_subscribe_never_leave_a_divergent_claim() {
	let Some(h) = harness().await else { return };
	let users = PgUsers::new(h.pool.clone());
	let subs: Arc<dyn SubscriptionRepository> = Arc::new(PgSubscriptions::new(h.pool.clone()));
	let withdrawals: Arc<dyn piggybank_core::ports::WithdrawalRepository> = Arc::new(PgWithdrawals::new(h.pool.clone()));
	let policy = PgOutflowPolicy::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let positions = PgFundPositions::new(h.pool.clone());
	let user = active_user(&h.pool, &users).await;
	let service = registered_service(&h).await;
	let now = now_unix();
	let user_claim = LedgerAccountKey::UserClaim(user);
	let service_claim = LedgerAccountKey::ServiceClaim(service.clone());
	let network = Network::Bep20;

	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, network, usdt("100")).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Fire both at once; the shared `users` advisory lock inside each `open` serializes the two
	// commits so they can never interleave a half-applied write. Both port bundles are bound
	// first: each future borrows its own, so neither may be a temporary of the `let` that
	// builds the future.
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let withdrawal_ports = withdrawal_app::WithdrawalPorts {
		withdrawals: withdrawals.as_ref(),
		ledger: h.ledger.as_ref(),
		custody: &StubCustody,
		relay: &h.notify,
	};
	let admission = withdrawal_app::AdmissionGates {
		policy: &policy,
		configured: &Network::ALL,
		kyc: KycGate::ENFORCED,
	};
	let sub_fut = funds_app::subscribe(&fund_ports, subs.as_ref(), user, service.clone(), usdt("80"), now);
	let wd_fut = withdrawal_app::request_withdrawal(&withdrawal_ports, &admission, WithdrawalId::new(), user, network, destination(network), usdt("80"));
	let (sub_res, wd_res) = tokio::join!(sub_fut, wd_fut);

	// At least one must succeed (100 covers a single 80-spend); the relay then applies the
	// serialized reservations and parks the over-commit.
	assert!(sub_res.is_ok() || wd_res.is_ok(), "at least one request must succeed (the claim covers 80)");
	// The park IS what this test pins, so the one-pass drain stays here: `drain_to_quiescence`
	// reports a park raised during the call as a failure.
	h.relay.drain().await;

	// Exactly one 80-spend landed — the other parked, never half-applied — so the claim shows
	// 20 spendable regardless of which won the relay race, and never goes negative (TB's flag).
	// `cash_in` is the cash a *winning* subscribe moved into the fund (zero if it parked): the
	// user's posted balance dropped only by a cash leg that actually moved (a withdrawal reserve
	// only locks, leaving posted at 100).
	let bal = h.ledger.balance(&user_claim).await.unwrap();
	let cash_in = claim(&h, &service_claim).await;
	assert_eq!(
		Usdt::from_base_units(bal.available()),
		usdt("20"),
		"exactly one 80-spend applied; the over-commit parked, nothing stranded"
	);
	assert_eq!(
		Usdt::from_base_units(bal.posted),
		usdt("100").checked_sub(cash_in).unwrap(),
		"posted dropped only by a cash leg that actually moved"
	);

	// The BANK-MONEY-1 invariant under contention: cost_basis is present IFF the subscribe's
	// cash leg posted (the fund's `service` claim holds the 80). A parked subscribe leaves no
	// phantom basis.
	let basis = cost_basis(&positions, user, &service).await;
	assert_eq!(basis.is_some(), !cash_in.is_zero(), "cost_basis exists iff the subscribe cash leg posted — never a phantom");
	if let Some(b) = basis {
		assert_eq!(b, cash_in, "the recorded basis matches the cash that actually entered the fund");
	}
}

// BANK-MONEY-3: two queued redemptions on ONE position settling back-to-back must reduce the
// average-cost basis by COMPOUNDING fractions, not each dividing by the same pre-burn units.
// The old code read units_held live from TB (`holding.posted`), which lags the async burn, so
// every settle divided by the gross 100 → an under-reduced (inflated) basis. The fix divides
// by the position's own projection-tracked units (decremented under the settle lock). Subscribe
// 100 @ NAV 1 → basis 100, units 100. Mark NAV to 4 so each 30-unit redemption (120 cash > the
// 100 fund claim) QUEUES; top the fund up, then settle both. Sequential-correct:
//   settle 30: basis = trunc(100 × 70/100) = 70, units 70
//   settle 30: basis = trunc(70  × 40/70 ) = 40, units 40
// The relay-lagging bug would compute 70 then trunc(70 × 70/100) = 49 — over-stated by 9.
#[tokio::test]
async fn back_to_back_settles_compound_the_cost_basis_reduction() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let positions = PgFundPositions::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();

	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("100"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("100"), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cost_basis(&positions, user, &service).await, Some(usdt("100")), "basis seeded by the subscribe");
	assert_eq!(tracked_units(&h.pool, user, &service).await, Some(shares("100")), "units tracked on the projection");

	// Mark NAV to 4 (AUM 400 / 100 units, a +300% move written through the shared writer —
	// what an executed owners' override records) so each 30-unit redemption prices to 120
	// cash — above the fund's 100 claim — and stays Queued (no auto-settle).
	funds_app::record_valuation(&nav_repo, h.ledger.as_ref(), ValuationId::new(), service.clone(), usdt("400"), "op")
		.await
		.unwrap();
	let r1 = funds_app::request_redemption(&fund_ports, &reds, user, service.clone(), shares("30"), now).await.unwrap();
	let r2 = funds_app::request_redemption(&fund_ports, &reds, user, service.clone(), shares("30"), now).await.unwrap();
	assert_eq!(r1.state(), RedemptionState::Queued, "short fund queues the first redemption");
	assert_eq!(r2.state(), RedemptionState::Queued, "short fund queues the second redemption");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Top the fund up so both settles' payouts clear the relay pre-check (2 × 120 = 240).
	realize_profit(&h, &service, usdt("140")).await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Settle both back-to-back — the under-reduction bug surfaces on the SECOND settle.
	funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, r1.id(), now).await.unwrap();
	funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, r2.id(), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	assert_eq!(
		cost_basis(&positions, user, &service).await,
		Some(usdt("40")),
		"compounded reduction (100→70→40), not the relay-lagging 49"
	);
	assert_eq!(
		tracked_units(&h.pool, user, &service).await,
		Some(shares("40")),
		"tracked units decremented per settle (100→70→40)"
	);
	assert_eq!(
		units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await,
		shares("40"),
		"TB holding agrees: 60 units burned"
	);
}

// Issue #41 mode 1: `SettleRedemption` is documented idempotent, so a repeat settle (an
// invited operator retry, or the loser of the auto-settle vs operator race) must leave the
// cost-basis reduction applied exactly once. The old code re-ran the reduction on the no-op
// transition: from basis 70 / units 70 after the real settle of 30 units, a repeat would
// re-reduce to trunc(70 × 40/70) = 40 — and a repeat of a FULL redemption would zero the
// basis under live units, showing the whole position as phantom profit. The repeat is
// exercised at both layers: the application command (which short-circuits without the NAV
// staleness check) and the row-locked repository settle (the race authority).
#[tokio::test]
async fn a_repeat_settle_reduces_the_cost_basis_exactly_once() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let positions = PgFundPositions::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();

	// AUM 400 → NAV 4, so the 30-unit redemption prices to 120 cash > the 100 fund claim
	// and queues; top the fund up, then settle it — once for real.
	let id = queued_short_redemption(&h, &subs, &reds, &nav_repo, user, &service, now, "400", "30").await;
	realize_profit(&h, &service, usdt("100")).await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, id, now).await.unwrap();
	assert_eq!(cost_basis(&positions, user, &service).await, Some(usdt("70")), "one settle: basis 100 → 70");
	assert_eq!(tracked_units(&h.pool, user, &service).await, Some(shares("70")), "one settle: units 100 → 70");

	// The application-layer retry: still Completed, the projection untouched — and no NAV
	// staleness check, so an idempotent retry succeeds even long after the mark.
	let repeat = funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, id, now + funds_app::MAX_NAV_AGE_SECS + 100)
		.await
		.unwrap();
	assert_eq!(repeat.state(), RedemptionState::Completed, "a repeat settle stays the idempotent no-op");

	// The repository-layer retry (what a raced concurrent settle hits under the row lock).
	let raced = reds.settle(id, Nav::SEED).await.unwrap();
	assert_eq!(raced.state(), RedemptionState::Completed, "the locked repo settle is the same no-op");
	assert_eq!(cost_basis(&positions, user, &service).await, Some(usdt("70")), "repeat settles must not re-reduce the basis");
	assert_eq!(tracked_units(&h.pool, user, &service).await, Some(shares("70")), "repeat settles must not re-decrement the units");
}

// Issue #41 mode 2: `fund_positions` is written by the relay AFTER the subscribe mint posts,
// while the redeem is admitted off the TB balance — so a settle can outrun the position's
// first projection write. The old reduction was a silent no-op on the absent row; the
// projection then landed the full basis over units already burned — a permanent overstate.
// The settle must instead refuse (`Conflict`, rolled back, still queued) until the projection
// tracks the redeemed units, and the auto-settle inside the redeem request must degrade that
// refusal to the accepted-and-queued redemption, not an error. Minting straight on the TB
// ledger reproduces the projection-lag state deterministically.
#[tokio::test]
async fn settle_refuses_reduction_until_the_subscribe_projection_lands() {
	let Some(h) = harness().await else { return };
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let positions = PgFundPositions::new(h.pool.clone());
	let user = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();

	// 100 units in TB with the projection row still absent — the mid-race state. The fund
	// claim is liquid (100), so the redeem request will attempt the immediate auto-settle.
	let mint = LedgerTransfer {
		id: Uuid::new_v4().as_u128(),
		debit: LedgerAccountKey::UserShares(service.clone(), user),
		credit: LedgerAccountKey::SharesOutstanding(service.clone()),
		amount: shares("100").base_units(),
		code: TransferCode::ShareMint,
		reference: 0,
	};
	h.ledger.post(&mint).await.unwrap();
	realize_profit(&h, &service, usdt("100")).await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// The auto-settle loses to the missing projection — the redeem is still accepted, queued.
	let r = funds_app::request_redemption(&fund_ports, &reds, user, service.clone(), shares("30"), now).await.unwrap();
	assert_eq!(r.state(), RedemptionState::Queued, "the raced auto-settle degrades to accepted-and-queued");
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// An operator settle before the projection lands is refused and fully rolled back.
	let err = funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, r.id(), now).await.unwrap_err();
	assert!(matches!(err, DomainError::Conflict(_)), "projection lag refuses the settle, got {err:?}");
	assert_eq!(reds.find_by_id(r.id()).await.unwrap().unwrap().state(), RedemptionState::Queued, "the refused settle rolled back");
	assert!(cost_basis(&positions, user, &service).await.is_none(), "nothing written against the absent row");

	// The projection lands (what the relay's `project_subscription` upsert would write)…
	sqlx::query("INSERT INTO fund_positions (user_id, service, cost_basis, units, high_water_mark) VALUES ($1, $2, $3, $4, $5)")
		.bind(user.raw())
		.bind(service.as_str())
		.bind(usdt("100").base_units().to_string())
		.bind(shares("100").base_units().to_string())
		.bind(Nav::SEED.base_units().to_string())
		.execute(&h.pool)
		.await
		.unwrap();

	// …and the retried settle now applies exactly once, against the full denominator.
	let settled = funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, r.id(), now).await.unwrap();
	assert_eq!(settled.state(), RedemptionState::Completed);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(cost_basis(&positions, user, &service).await, Some(usdt("70")), "reduced against the landed projection (100 → 70)");
	assert_eq!(tracked_units(&h.pool, user, &service).await, Some(shares("70")), "tracked units decremented once (100 → 70)");
	assert_eq!(
		units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await,
		shares("70"),
		"TB holding agrees: 30 burned"
	);
	assert_eq!(claim(&h, &LedgerAccountKey::UserClaim(user)).await, usdt("30"), "the payout (30 × seed NAV) reached the user");
}

// ── access: who a product deals with ─────────────────────────────────────────

/// The money side of "closed by default". A product an operator opened but has not
/// opened TO this investor refuses their subscription with its own kind of error —
/// distinguishable from "not open" (validation) and from the cap (validation) — and,
/// crucially, before any money moves. A grant to `invest` then lets the same request
/// through; a grant to `view` does not.
#[tokio::test]
async fn subscribe_refuses_an_investor_the_product_is_not_open_to_until_granted() {
	let Some(h) = harness().await else { return };
	let users = PgUsers::new(h.pool.clone());
	let subs = PgSubscriptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	// Grants reference `users`, so these are real rows — as they are in production.
	let investor = active_user(&h.pool, &users).await;
	let operator = active_user(&h.pool, &users).await;
	let service = registered_locked_service(&h).await;
	let now = now_unix();

	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), investor, Network::Bep20, usdt("100"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let err = funds_app::subscribe(&fund_ports, &subs, investor, service.clone(), usdt("50"), now).await.unwrap_err();
	assert!(
		matches!(err, DomainError::Precondition(ref m) if m.contains("grant")),
		"locked is its own kind of refusal: {err:?}"
	);
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(claim(&h, &LedgerAccountKey::UserClaim(investor)).await, usdt("100"), "refused before any money moved");
	assert!(units(&h, &LedgerAccountKey::SharesOutstanding(service.clone())).await.is_zero(), "nothing minted");

	// `view` is a grant, but not enough to put money in.
	h.allocations.grant_access(&service, investor, AllocationAccess::View, operator).await.unwrap();
	let err = funds_app::subscribe(&fund_ports, &subs, investor, service.clone(), usdt("50"), now).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "{err:?}");

	// A repeat grant overwrites the level, and `invest` opens the door for this investor.
	h.allocations.grant_access(&service, investor, AllocationAccess::Invest, operator).await.unwrap();
	funds_app::subscribe(&fund_ports, &subs, investor, service.clone(), usdt("50"), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units(&h, &LedgerAccountKey::UserShares(service.clone(), investor)).await, shares("50"), "minted once granted");

	// The product is still locked for everyone else — the grant is per investor.
	let stranger = UserId::new();
	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), stranger, Network::Bep20, usdt("10"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	let err = funds_app::subscribe(&fund_ports, &subs, stranger, service.clone(), usdt("10"), now).await.unwrap_err();
	assert!(matches!(err, DomainError::Precondition(_)), "{err:?}");
}

/// The other way in: raising the product's DEFAULT to `invest` admits everyone, no
/// grant needed — and lowering it back locks new money without touching what is held.
#[tokio::test]
async fn an_invest_default_admits_anyone_and_lowering_it_never_traps_a_holder() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let user = UserId::new();
	let service = registered_locked_service(&h).await;
	let now = now_unix();

	balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("100"))
		.await
		.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	h.allocations.set_access(&service, AllocationAccess::Invest).await.unwrap();
	funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("100"), now).await.unwrap();
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(units(&h, &LedgerAccountKey::UserShares(service.clone(), user)).await, shares("100"));

	// The operator locks the product down completely — hidden from everyone without a
	// grant. New money stops…
	h.allocations.set_access(&service, AllocationAccess::Hidden).await.unwrap();
	let err = funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("1"), now).await.unwrap_err();
	assert!(
		matches!(err, DomainError::NotFound { entity: "allocation", .. }),
		"a hidden product answers as unregistered: {err:?}"
	);

	// …but the holder gets out in full: redemptions never consult access. Refusing here
	// would lock 100 units of real money inside a product the investor can no longer see.
	let redemption = funds_app::request_redemption(&fund_ports, &reds, user, service.clone(), shares("100"), now)
		.await
		.expect("a locked product must still redeem");
	assert_eq!(redemption.state(), RedemptionState::Completed, "the fund's own claim covers it, so it settles at once");
	common::drain_to_quiescence(&h.relay, &h.pool).await;
	assert_eq!(claim(&h, &LedgerAccountKey::UserClaim(user)).await, usdt("100"), "the investor's cash came back");
}

/// The redeem cooldown at REQUEST: whoever posted the fund's mark cannot redeem from it
/// for `VALUATION_REDEEM_COOLDOWN_SECS`, while another investor redeems at that price.
///
/// The mark is recorded with `posted_by` spelled exactly as the RPC records it: the token's
/// `sub`, a hyphenated UUID that `caller_id` parses back into this same `UserId` — so this
/// also pins that the direct path's spelling and the cooldown's comparison agree.
#[tokio::test]
async fn a_valuation_poster_cannot_redeem_from_that_fund_inside_the_cooldown() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let poster = UserId::new();
	let other = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();
	for user in [poster, other] {
		balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("100"))
			.await
			.unwrap();
		common::drain_to_quiescence(&h.relay, &h.pool).await;
		funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("100"), now).await.unwrap();
		common::drain_to_quiescence(&h.relay, &h.pool).await;
	}
	// 200 units outstanding; AUM 240 marks the fund at 1.2, inside the guard.
	let sub = poster.raw().to_string();
	assert_eq!(sub, poster.to_string(), "the cooldown compares the UserId's own spelling against the recorded sub");
	funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("240"), &sub, now)
		.await
		.unwrap();

	let err = funds_app::request_redemption(&fund_ports, &reds, poster, service.clone(), shares("10"), now).await.unwrap_err();
	assert!(matches!(&err, DomainError::Precondition(reason) if reason.contains("posted a valuation")), "got {err:?}");
	assert_eq!(
		units_available(&h, &LedgerAccountKey::UserShares(service.clone(), poster)).await,
		shares("100"),
		"refused before anything was reserved"
	);
	// The other investor redeems at the poster's price, and the fund covers it at once.
	let r = funds_app::request_redemption(&fund_ports, &reds, other, service.clone(), shares("10"), now).await.unwrap();
	assert_eq!(r.state(), RedemptionState::Completed);
	// Past the cooldown the refusal is no longer the cooldown's: what stands in the way then
	// is the ordinary staleness guard on a week-old mark, a different error on purpose.
	// Measured from the mark's DB-stamped `posted_at`, not from `now` — the mark landed some
	// seconds after `now` was taken, and the cooldown is measured from the mark.
	let posted_at = nav_repo.current(&service).await.unwrap().unwrap().posted_at_unix;
	let later = posted_at + funds_app::VALUATION_REDEEM_COOLDOWN_SECS + 1;
	let err = funds_app::request_redemption(&fund_ports, &reds, poster, service.clone(), shares("10"), later).await.unwrap_err();
	assert!(matches!(err, DomainError::Validation(_)), "the cooldown lifted; got {err:?}");
}

/// The redeem cooldown at SETTLE: a redemption queued BEFORE its owner marked the fund is
/// refused when the settle comes — settle is where the cash is priced, and the queue must
/// not be a way to get in ahead of one's own mark.
#[tokio::test]
async fn a_queued_redemption_is_refused_at_settle_once_its_owner_has_marked_the_fund() {
	let Some(h) = harness().await else { return };
	let subs = PgSubscriptions::new(h.pool.clone());
	let reds = PgRedemptions::new(h.pool.clone());
	let nav_repo = PgNav::new(h.pool.clone());
	let fund_ports = funds_app::FundPorts {
		allocations: &h.allocations,
		ledger: h.ledger.as_ref(),
		nav: &nav_repo,
		relay: &h.notify,
	};
	let poster = UserId::new();
	let other = UserId::new();
	let service = registered_service(&h).await;
	let now = now_unix();
	for user in [poster, other] {
		balance_app::record_deposit(&h.deposits, &h.notify, unique_tx_ref(), user, Network::Bep20, usdt("100"))
			.await
			.unwrap();
		common::drain_to_quiescence(&h.relay, &h.pool).await;
		funds_app::subscribe(&fund_ports, &subs, user, service.clone(), usdt("100"), now).await.unwrap();
		common::drain_to_quiescence(&h.relay, &h.pool).await;
	}
	// Someone else marks the fund at 10 (AUM 2000 / 200 units — past the guard, through the
	// shared writer), so a 50-unit redemption prices to 500 against a 200 claim and queues.
	funds_app::record_valuation(&nav_repo, h.ledger.as_ref(), ValuationId::new(), service.clone(), usdt("2000"), "op")
		.await
		.unwrap();
	let queued_by_poster = funds_app::request_redemption(&fund_ports, &reds, poster, service.clone(), shares("50"), now).await.unwrap();
	let queued_by_other = funds_app::request_redemption(&fund_ports, &reds, other, service.clone(), shares("50"), now).await.unwrap();
	assert_eq!(queued_by_poster.state(), RedemptionState::Queued);
	assert_eq!(queued_by_other.state(), RedemptionState::Queued);
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	// Now the poster marks the fund themselves (10 → 10.5, inside the guard) and the fund is
	// topped up so either settle could pay.
	funds_app::post_fund_valuation(&h.allocations, &nav_repo, h.ledger.as_ref(), service.clone(), usdt("2100"), &poster.to_string(), now)
		.await
		.unwrap();
	realize_profit(&h, &service, usdt("1000")).await;
	common::drain_to_quiescence(&h.relay, &h.pool).await;

	let err = funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, queued_by_poster.id(), now)
		.await
		.unwrap_err();
	assert!(matches!(&err, DomainError::Precondition(reason) if reason.contains("posted a valuation")), "got {err:?}");
	assert_eq!(
		reds.find_by_id(queued_by_poster.id()).await.unwrap().unwrap().state(),
		RedemptionState::Queued,
		"refused, not failed: it settles once the cooldown ages out"
	);
	let settled = funds_app::settle_redemption(&reds, &nav_repo, h.ledger.as_ref(), &h.notify, queued_by_other.id(), now)
		.await
		.unwrap();
	assert_eq!(settled.state(), RedemptionState::Completed, "the other investor's redemption settles at the poster's price");
}
