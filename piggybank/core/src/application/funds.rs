//! Fund (share) use cases — the service currency.
//!
//! NAV is **derived**, not posted directly: an operator posts a fund's total AUM and the
//! handler reads `units_outstanding` live from TigerBeetle to compute
//! `NAV = AUM / units_outstanding` (frozen until the next mark). Subscribe/redeem
//! (later slices) deal on the latest mark — a deliberate *backward-pricing* tradeoff,
//! guarded by a **staleness** check; the operator post is guarded by a **move** check,
//! because the AUM input is the most dangerous seam in the system ("trusted" ≠ "safe").
//!
//! The reserved `fee` and `fund` allocations (#245) take no mark at all: their price is
//! **computed** from what they hold — the products' fee classes at those products' NAV
//! plus the cash on their own claim, over their own supply — by [`nav_of`], the one
//! reader every price in this module and beyond goes through.

use domain::{
	allocations::AllocationAccess,
	balance::{LedgerAccountKey, ServiceId, ValuationId},
	error::DomainError,
	issuance::UnitHolder,
	money::{Nav, Shares, Usdt},
	redemptions::{Redemption, RedemptionId, RedemptionState},
	subscriptions::{Subscription, SubscriptionId},
	users::UserId,
};
use tokio::sync::Notify;

use crate::{
	application::allocations as allocations_app,
	ports::{
		FundPositionReader, RedemptionRepository, SubscriptionRepository, UnitFlow,
		allocations::{AllocationRecord, AllocationRegistry},
		ledger::{ClaimCovers, HoldingScope, Ledger, SpendGuard},
		nav::{NavMarks, Valuation},
	},
};

/// A derived NAV that moves more than this (percent) from the previous mark, or from the
/// mark anchoring the rolling [`NAV_MOVE_WINDOW_SECS`] window, is rejected — the
/// fat-finger guard on the AUM trust seam, and since banking#232 the ONLY guard a single
/// poster faces: there is no per-request override. A larger move goes to the owners
/// (`consilium::open_valuation_override`).
pub const MAX_NAV_MOVE_PCT: u128 = 50;
/// The rolling window the move cap is also measured across.
///
/// WHY A SECOND MEASUREMENT. Against the previous mark alone, the cap is a rate limit with
/// no rate: +49% seventeen times in an afternoon compounds to a 900× NAV while every single
/// post is "within 50%". Measuring the same cap against the newest mark at least this old
/// (or the fund's first mark, when none is) bounds what one poster can move the price by
/// in a week to the cap itself, however many steps they take.
pub const NAV_MOVE_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;
/// How long after posting a valuation for a fund the poster may not redeem from it.
///
/// The redeem settles at the price the poster set; a cooldown means the person who moved
/// the price is not the person who cashes out at it, within the window the move guard
/// measures. Fee settlement is deliberately not gated: its cash lands in `fee`, whose only
/// exit is already a consilium.
pub const VALUATION_REDEEM_COOLDOWN_SECS: i64 = 7 * 24 * 60 * 60;
/// A mark older than this (seconds) is stale; subscribe/redeem refuse to deal on it
/// rather than price off a drifted NAV (the backward-pricing arbitrage guard). 24h for v1.
pub const MAX_NAV_AGE_SECS: i64 = 24 * 60 * 60;
/// The most marks one [`fund_nav_history`] answer carries. Marks are posted by hand, a
/// few a week at most, so this is years of history for any real fund; a window with more
/// keeps the NEWEST and reports itself truncated rather than paging or refusing.
pub const MAX_NAV_HISTORY_MARKS: usize = 2000;
/// A user's position in one fund, assembled from the live unit balances (TigerBeetle),
/// the current NAV, and the cost-basis projection. `value = (units + units_in_orders) ×
/// nav`; P&L is `value − cost_basis` (computed at the wire boundary, where a signed value
/// is natural).
pub struct PositionView {
	pub service: ServiceId,
	/// Units free in the holding — redeemable, sellable.
	pub units: Shares,
	/// Units committed to the holder's resting sell orders on the book. Still theirs and
	/// still valued, but not free until the order fills or is cancelled.
	pub units_in_orders: Shares,
	pub nav: Nav,
	pub value: Usdt,
	pub cost_basis: Usdt,
	/// Unix seconds of the NAV mark used (0 when on the bootstrap seed NAV).
	pub nav_as_of: i64,
}

/// A fund's current price and freshness for display.
pub struct FundNavView {
	pub service: ServiceId,
	pub nav: Nav,
	/// The last posted AUM, or `None` when the fund is still on the seed NAV.
	pub aum: Option<Usdt>,
	/// The **settled** supply — the denominator NAV is derived against.
	pub units_outstanding: Shares,
	/// The allocation's authorised unit supply.
	pub unit_cap: Shares,
	/// Units still issuable, measured the way [`subscribe`] measures them (settled plus
	/// in-flight). Reported rather than left to the caller to subtract, so a screen can
	/// never offer headroom the subscribe gate would then refuse.
	pub remaining_capacity: Shares,
	/// Unix seconds of the latest mark (0 = never marked / seed).
	pub posted_at: i64,
	pub stale: bool,
}

/// A fund's valuation log over a window plus the caller's participation through it —
/// the two series of the performance chart.
#[derive(Debug)]
pub struct FundNavHistoryView {
	pub service: ServiceId,
	/// Oldest first, within the window, at most [`MAX_NAV_HISTORY_MARKS`].
	pub marks: Vec<Valuation>,
	/// Oldest first; see [`participation_series`] for which instants get a point.
	pub participation: Vec<ParticipationPoint>,
	/// More marks fell in the window than the cap; the oldest were dropped.
	pub truncated: bool,
}

/// The caller's holding valued at one instant: the units they held then × the NAV in
/// force then.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParticipationPoint {
	pub at_unix: i64,
	pub value: Usdt,
}

/// The driven ports a dealing use-case borrows: the registry it gates on, the ledger its
/// Read-First checks read, the marks it prices at, and the relay it nudges once the
/// control-plane commit lands. Bundled so a use-case's own parameters are its *request* —
/// who, which fund, how much, as of when — rather than the wiring the composition root
/// injects. A plain borrow-holder: it owns nothing, decides nothing, and outlives nothing.
pub struct FundPorts<'a> {
	/// The registry of investable products — the gate every deal resolves `service` through.
	pub allocations: &'a dyn AllocationRegistry,
	/// The money gateway (TigerBeetle): the authoritative balances the Read-First checks read.
	pub ledger: &'a dyn Ledger,
	/// The valuation marks a deal is priced at, staleness guard included.
	pub nav: &'a dyn NavMarks,
	/// Nudged after the commit so the outbox relay moves money promptly.
	pub relay: &'a Notify,
}

/// One allocation's price as every reader sees it: the NAV, the AUM it prices (the
/// posted mark's, or the computed value of a reserved allocation), and the instant the
/// price is as of — `0` when nothing has ever been marked, which is also "never stale".
#[derive(Clone, Copy, Debug)]
pub struct NavQuote {
	pub nav: Nav,
	/// `None` for a product still on the seed NAV; always `Some` for a reserved
	/// allocation, whose value is a sum, not a mark.
	pub aum: Option<Usdt>,
	/// A product: its latest mark's `posted_at`. A reserved allocation: the OLDEST mark
	/// among the products it holds units of — its price is only as fresh as its stalest
	/// input — or `0` when none of them has been marked (cash and seed-priced units).
	pub posted_at_unix: i64,
}

impl NavQuote {
	fn seed() -> Self {
		Self {
			nav: Nav::SEED,
			aum: None,
			posted_at_unix: 0,
		}
	}

	/// Whether the price is older than [`MAX_NAV_AGE_SECS`] at `now`. A never-marked
	/// price has nothing to be stale against.
	pub fn is_stale(&self, now_unix: i64) -> bool {
		self.posted_at_unix != 0 && now_unix.saturating_sub(self.posted_at_unix) > MAX_NAV_AGE_SECS
	}
}

/// The price of `service` — THE reader of NAV, for every position, deal and screen.
///
/// A product's is its latest mark (the seed NAV before the first). A reserved
/// allocation's (`fee`, `fund`) is **computed**, never posted: the value of the product
/// units it holds — for `fee`, every product's fee class, each at that product's own
/// NAV — plus the cash on its claim, divided by its own supply. With no units
/// outstanding the price is the seed NAV, so the first units issued to a holder are
/// worth exactly what stands behind them and a later `SharesOutstanding` of zero can
/// never divide anything. The holdings are read from the ledger, not the registry: what
/// the allocation is worth is what it holds, whether or not a product is still listed.
pub async fn nav_of(nav: &dyn NavMarks, ledger: &dyn Ledger, service: &ServiceId) -> Result<NavQuote, DomainError> {
	if !service.is_reserved() {
		return marked_quote(nav, service).await;
	}
	let mut value = Usdt::from_base_units(ledger.balance(&LedgerAccountKey::ServiceClaim(service.clone())).await?.posted);
	let mut oldest_mark = 0i64;
	for (key, units) in ledger.share_holdings(&HoldingScope::Holder(UnitHolder::Allocation(service.clone()))).await? {
		if units == 0 {
			continue;
		}
		let Some((product, _)) = UnitHolder::of_holding(&key) else { continue };
		// A reserved allocation holds product units only (the holder graph is reserved →
		// product, one hop), so a product's price is always a mark, never another sum.
		let product_price = marked_quote(nav, &product).await?;
		let holding = product_price.nav.value(Shares::from_base_units(units))?;
		value = value.checked_add(holding).ok_or_else(|| DomainError::Repository("allocation value overflows".into()))?;
		if product_price.posted_at_unix != 0 && (oldest_mark == 0 || product_price.posted_at_unix < oldest_mark) {
			oldest_mark = product_price.posted_at_unix;
		}
	}
	let outstanding = Shares::from_base_units(ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await?.posted);
	let price = if outstanding.is_zero() { Nav::SEED } else { Nav::from_aum(value, outstanding)? };
	Ok(NavQuote {
		nav: price,
		aum: Some(value),
		posted_at_unix: oldest_mark,
	})
}

/// A product's price: its latest mark, or the seed NAV before the first.
async fn marked_quote(nav: &dyn NavMarks, service: &ServiceId) -> Result<NavQuote, DomainError> {
	Ok(match nav.current(service).await? {
		Some(v) => NavQuote {
			nav: v.nav,
			aum: Some(v.aum),
			posted_at_unix: v.posted_at_unix,
		},
		None => NavQuote::seed(),
	})
}

/// The current NAV plus whether it is fresh enough to deal on (`now − posted_at ≤
/// MAX_NAV_AGE_SECS`). A fund with no mark yet uses the seed NAV and is always fresh
/// (nothing to be stale against); a reserved allocation is as fresh as the stalest
/// product mark its price depends on. Subscribe/redeem call this before pricing.
pub async fn dealing_nav(nav: &dyn NavMarks, ledger: &dyn Ledger, service: &ServiceId, now_unix: i64) -> Result<Nav, DomainError> {
	let quote = nav_of(nav, ledger, service).await?;
	if quote.is_stale(now_unix) {
		return Err(DomainError::Validation("fund nav is stale — a fresh valuation is required before dealing".into()));
	}
	Ok(quote.nav)
}

/// One allocation as `caller` may read it: as [`allocations_app::get_for`] answers — or,
/// when that answer is `NotFound` because the product is hidden from them, as its
/// **holder**. Whoever holds units of an allocation reads its title and price like any
/// other holder, however the catalog treats them: the reserved `fee` and `fund`
/// allocations are hidden from everyone and held by people (#245), and a person's own
/// position cannot be a product that does not exist. Holding is a ledger fact, read
/// live — free units or units resting in a sell. The catalog listing is untouched.
pub async fn allocation_for_holder(
	allocations: &dyn AllocationRegistry,
	ledger: &dyn Ledger,
	service: &ServiceId,
	caller: UserId,
	unrestricted: bool,
) -> Result<AllocationRecord, DomainError> {
	match allocations_app::get_for(allocations, service, caller, unrestricted).await {
		Err(DomainError::NotFound { .. }) if holds_units(ledger, service, caller).await? => allocations.find_for(service, caller).await?.ok_or_else(|| DomainError::NotFound {
			entity: "allocation",
			id: service.to_string(),
		}),
		answer => answer,
	}
}

async fn holds_units(ledger: &dyn Ledger, service: &ServiceId, user: UserId) -> Result<bool, DomainError> {
	let free = ledger.balance(&LedgerAccountKey::UserShares(service.clone(), user)).await?.posted;
	let escrowed = ledger.balance(&LedgerAccountKey::BookShares(service.clone(), user)).await?.posted;
	Ok(free != 0 || escrowed != 0)
}

/// A user subscribes `cash` of their free balance into `service`, minting
/// `floor(cash / NAV)` units at the current (fresh) NAV. Read-First confirms the
/// spendable unified claim covers the cash (TigerBeetle's flag is the backstop); the
/// staleness guard refuses to deal on a drifted mark. The relay then posts the cash move
/// (`Dr UserClaim / Cr ServiceClaim`) and the unit mint (`Dr UserShares / Cr
/// SharesOutstanding`) — cash-leg first, so an insufficient claim parks before any mint.
///
/// The **registry gate runs first**, before any balance read or pricing: `service` must
/// be a registered, `open` allocation that this user holds `invest` access to — by the
/// product's default or by a grant. It is what stops a user from minting a fund out of
/// an arbitrary slug — previously the only check was the slug's shape, and a service
/// with no valuation bootstrapped silently at the seed NAV — and what keeps a product
/// an operator has not opened to this investor from taking their money. The access
/// refusal is its own kind ([`DomainError::Precondition`]) so a client can tell "ask an
/// operator" from "the product is closed" from "over the cap".
///
/// A **reserved allocation** is refused before either (#245): `fee` and `fund` are the
/// platform's own money, and a seat at them is the owners' quorum's to give — a holder
/// grant, or the seed of a chain-proven arrival — never something cash buys. The catalog
/// hides them from everyone, which would refuse this anyway; the explicit check is what
/// keeps a grant of `invest` on them (itself refused in `allocations`) from ever being a
/// way in. The seed prices through [`price_fund_seed`], which is not this path.
///
/// The **supply gate** runs second, once the mint has been priced and is therefore
/// known: `issued + minting` must fit the allocation's unit cap. Like the cash check
/// above it is Read-First — and unlike the cash check it has no TigerBeetle backstop
/// behind it, because a ledger can refuse to go below zero but has no notion of a
/// ceiling. Two consequences, both deliberate:
///
/// * Concurrent subscribes can each read the same `issued` and both pass, so the cap can
///   be overshot by what is in flight at that instant.
/// * `issued` is read from the ledger, which the relay writes *after* the control-plane
///   commit — so a subscription committed moments ago may not be counted yet.
///
/// The cap is therefore an **issuance gate, not an invariant**: it reliably stops a fund
/// from running away, and does not promise the last unit is exact. Making it exact would
/// mean reconstructing the outstanding supply in Postgres — a second source of truth for
/// a figure TigerBeetle already owns, which is the trade this architecture refuses
/// everywhere else.
pub async fn subscribe(ports: &FundPorts<'_>, subscriptions: &dyn SubscriptionRepository, user: UserId, service: ServiceId, cash: Usdt, now_unix: i64) -> Result<Subscription, DomainError> {
	if service.is_reserved() {
		return Err(DomainError::Forbidden(format!(
			"units of the reserved '{service}' allocation are seated only by a holder grant or a seed, never bought"
		)));
	}
	let allocation = allocations_app::require_subscribable(ports.allocations, &service, user).await?;
	let covers = ClaimCovers {
		ledger: ports.ledger,
		key: LedgerAccountKey::UserClaim(user),
		amount: cash.base_units(),
		refusal: "insufficient available balance to subscribe",
	};
	// Asked now to refuse before pricing, and again by the repository under the claim lock.
	covers.check().await?;
	let price = dealing_nav(ports.nav, ports.ledger, &service, now_unix).await?;
	allocation.ensure_capacity(issued_units(ports.ledger, &service).await?, Shares::from_cash(cash, price)?)?;
	let mut subscription = Subscription::open(SubscriptionId::new(), user, service, cash, price)?;
	subscriptions.open(&mut subscription, &covers).await?;
	ports.relay.notify_one();
	Ok(subscription)
}

/// Price and gate a seed of the `fund` allocation — `depositor` subscribing `cash` into it
/// — without opening it: the caller records the chain-proven deposit the cash comes from
/// first, then opens what this returns (see `balance::seed_fund_capital`), so a refusal
/// here leaves nothing written.
///
/// The gates are [`subscribe`]'s minus two, each left out for a reason spelled here:
///
/// * **The catalog's access level.** `fund` is `hidden` from everyone (#245) — a holder
///   reads it through [`allocation_for_holder`], never through the catalog — so
///   `require_subscribable` would refuse every depositor. The state gate it also runs is
///   kept: a `fund` an operator closed takes no seed. `Invest` is asserted in its place
///   because the operator's permission to seed is the admission.
/// * **The Read-First balance check.** The cash is the deposit being recorded alongside,
///   not yet on the ledger; TigerBeetle's non-negative flag stays the backstop and a
///   parked cash leg leaves the money on the depositor's own claim.
///
/// The price is [`dealing_nav`], as for any deal — the seed NAV while `fund` has no supply,
/// its computed value per unit after — and the unit cap applies as to any subscription.
pub async fn price_fund_seed(ports: &FundPorts<'_>, id: SubscriptionId, depositor: UserId, cash: Usdt, now_unix: i64) -> Result<Subscription, DomainError> {
	let service = ServiceId::fund();
	let allocation = allocations_app::get(ports.allocations, &service).await?;
	allocation.ensure_subscribable(AllocationAccess::Invest)?;
	let price = dealing_nav(ports.nav, ports.ledger, &service, now_unix).await?;
	allocation.ensure_capacity(issued_units(ports.ledger, &service).await?, Shares::from_cash(cash, price)?)?;
	Subscription::open(id, depositor, service, cash, price)
}

/// Units the ledger considers issued for `service` — settled **plus in-flight inflow**.
///
/// Counting `pending` is the conservative direction for a ceiling: an unsettled mint is
/// supply that is on its way out, and treating it as absent would hand the same headroom
/// to two subscriptions. Pending *burns* (`locked`) are deliberately not subtracted —
/// those units still exist until the burn settles, and a queued redemption that is later
/// cancelled would otherwise have briefly re-opened capacity that was never free.
pub(crate) async fn issued_units(ledger: &dyn Ledger, service: &ServiceId) -> Result<Shares, DomainError> {
	let balance = ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await?;
	Ok(Shares::from_base_units(balance.posted.saturating_add(balance.pending)))
}

/// A user redeems `units` of `service` back to cash. Read-First confirms the user holds
/// the units (TigerBeetle's flag is the over-redeem backstop); the staleness guard
/// refuses to deal on a drifted mark. The redemption is **accepted and queued**: the
/// relay reserves a pending burn now, and the cash is priced + paid at **settle**. If the
/// fund's claim can already cover the payout, this settles immediately via a **separate**
/// command (never co-emitting `Requested`+`Settled`, which would race the burn reserve);
/// otherwise it stays `Queued` for an operator `settle_redemption` once the fund tops up.
///
/// The registry gate here is the **laxer** one: a `closed` allocation still redeems, and
/// the caller's access is never consulted, so neither winding a product down nor locking
/// it can trap an investor's units inside it.
///
/// A reserved allocation (`fee`, `fund`) is priced by [`nav_of`] and paid out of its own
/// claim like any product — but a shortfall there is **refused, never queued**: nobody
/// tops a reserved allocation up on request, its cash grows only as products settle
/// their fee classes, and a queue of the platform's own holders waiting on themselves
/// would be a queue nobody drains. The holder settles fee units first and asks again.
pub async fn request_redemption(
	ports: &FundPorts<'_>,
	redemptions: &dyn RedemptionRepository,
	user: UserId,
	service: ServiceId,
	units: Shares,
	now_unix: i64,
) -> Result<Redemption, DomainError> {
	allocations_app::require_redeemable(ports.allocations, &service).await?;
	refuse_recent_poster(ports.nav, ports.ledger, &service, user, now_unix).await?;
	let holding = ports.ledger.balance(&LedgerAccountKey::UserShares(service.clone(), user)).await?;
	if Shares::from_base_units(holding.available()) < units {
		return Err(DomainError::Validation("insufficient units to redeem".into()));
	}
	// Fresh NAV (staleness guard) — also the auto-settle liquidity estimate.
	let price = dealing_nav(ports.nav, ports.ledger, &service, now_unix).await?;
	let cash_out = price.value(units)?;
	let fund = ports.ledger.balance(&LedgerAccountKey::ServiceClaim(service.clone())).await?;
	let covered = Usdt::from_base_units(fund.available()) >= cash_out;
	if !covered && service.is_reserved() {
		return Err(DomainError::Validation(format!(
			"the '{service}' allocation's cash cannot cover this redemption — settle fee units into it first, or redeem fewer units"
		)));
	}
	let mut redemption = Redemption::request(RedemptionId::new(), user, service.clone(), units)?;
	redemptions.open(&mut redemption).await?;
	ports.relay.notify_one();
	// Accept-and-queue: settle now (as a separate command) iff the fund's claim can cover
	// the payout; else leave it queued for the treasury worker.
	if covered {
		// The settle can lose a race — to the relay's subscribe projection (rolled back,
		// stays queued for the operator) or to a concurrent cancel/fail. The redemption
		// was accepted either way, so a `Conflict` reports its actual current state
		// rather than surfacing an error for an already-opened redemption.
		return match settle_redemption(redemptions, ports.nav, ports.ledger, ports.relay, redemption.id(), now_unix).await {
			Err(DomainError::Conflict(_)) => redemptions.find_by_id(redemption.id()).await?.ok_or_else(|| DomainError::NotFound {
				entity: "redemption",
				id: redemption.id().to_string(),
			}),
			settled => settled,
		};
	}
	Ok(redemption)
}

/// Settle a queued redemption (the auto follow-on, or an operator once the fund is
/// liquid): prices the cash at the **settle-time** NAV (`units × NAV`) and pays it. The
/// relay posts the burn then the payout, guarded by a Read-First check on the fund claim.
/// The cost-basis reduction divides by the position's own projection-tracked units inside
/// the locked settle tx — not a live TB holding, which lags the async burn — so back-to-back
/// settles compound deterministically (BANK-MONEY-3), and it applies exactly once per
/// redemption (see [`crate::infrastructure::redemptions`]).
pub async fn settle_redemption(
	redemptions: &dyn RedemptionRepository,
	nav: &dyn NavMarks,
	ledger: &dyn Ledger,
	relay: &Notify,
	id: RedemptionId,
	now_unix: i64,
) -> Result<Redemption, DomainError> {
	let existing = redemptions.find_by_id(id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "redemption",
		id: id.to_string(),
	})?;
	// Completed is terminal, so this unlocked read is safe to short-circuit on: the
	// documented-idempotent retry must not re-check NAV freshness (a mark gone stale
	// since the real settle would fail it). Concurrent settles that both pass this
	// pre-check serialize on the repo's row lock, where the same guard is authoritative.
	if existing.state() == RedemptionState::Completed {
		return Ok(existing);
	}
	// Checked at settle too, not only at request: a queued redemption can outlive a mark
	// its owner posts later, and settle is where the cash is actually priced.
	refuse_recent_poster(nav, ledger, existing.service(), existing.user(), now_unix).await?;
	let price = dealing_nav(nav, ledger, existing.service(), now_unix).await?;
	let redemption = redemptions.settle(id, price).await?;
	relay.notify_one();
	Ok(redemption)
}

/// The redeem cooldown: refuse when `user` posted a mark for `service` within
/// [`VALUATION_REDEEM_COOLDOWN_SECS`]. `posted_by` is compared as the string the direct
/// RPC records — `claims.sub`, which `caller_id` parses as this same `UserId`, so the two
/// spellings agree by construction (pinned by an integration test).
///
/// A reserved allocation takes no mark of its own; its price is the marks of the products
/// it holds units of (see [`nav_of`]). So the cooldown on it is the cooldown on EVERY one of
/// those products (#245, H-3): a `fee` holder who marks a product up and redeems `fee` at
/// once cashes out at the price they set, exactly what the cooldown exists to stop, and the
/// same person settling the product's fee class into `fee` cash after their own mark is
/// the same move one step earlier (M-1) — hence `pub(crate)`, for `fees::settle_fee_shares`.
pub(crate) async fn refuse_recent_poster(nav: &dyn NavMarks, ledger: &dyn Ledger, service: &ServiceId, user: UserId, now_unix: i64) -> Result<(), DomainError> {
	let since = now_unix.saturating_sub(VALUATION_REDEEM_COOLDOWN_SECS);
	let subject = user.to_string();
	let days = VALUATION_REDEEM_COOLDOWN_SECS / (24 * 60 * 60);
	if !service.is_reserved() {
		if nav.posted_by_since(service, &subject, since).await? {
			return Err(DomainError::Precondition(format!(
				"you posted a valuation for this fund within the last {days} days — redemption is refused until it ages out"
			)));
		}
		return Ok(());
	}
	for (key, units) in ledger.share_holdings(&HoldingScope::Holder(UnitHolder::Allocation(service.clone()))).await? {
		if units == 0 {
			continue;
		}
		let Some((product, _)) = UnitHolder::of_holding(&key) else { continue };
		if nav.posted_by_since(&product, &subject, since).await? {
			return Err(DomainError::Precondition(format!(
				"you posted a valuation for '{product}', which the '{service}' allocation holds, within the last {days} days — dealing in it is refused until the mark ages out"
			)));
		}
	}
	Ok(())
}

/// Cancel a queued redemption (the calling user): the relay voids the burn, returning the
/// units. Ownership is checked here; the aggregate refuses to cancel once settled.
pub async fn cancel_redemption(redemptions: &dyn RedemptionRepository, relay: &Notify, id: RedemptionId, user: UserId) -> Result<Redemption, DomainError> {
	let existing = redemptions.find_by_id(id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "redemption",
		id: id.to_string(),
	})?;
	if existing.user() != user {
		return Err(DomainError::Forbidden("not your redemption".into()));
	}
	let redemption = redemptions.cancel(id).await?;
	relay.notify_one();
	Ok(redemption)
}

/// Fail a queued redemption (operator): the relay voids the burn, returning the units.
pub async fn fail_redemption(redemptions: &dyn RedemptionRepository, relay: &Notify, id: RedemptionId) -> Result<Redemption, DomainError> {
	let redemption = redemptions.fail(id).await?;
	relay.notify_one();
	Ok(redemption)
}

/// A user's redemptions (projection), newest first.
pub async fn list_redemptions(redemptions: &dyn RedemptionRepository, user: UserId) -> Result<Vec<Redemption>, DomainError> {
	redemptions.list_by_user(user).await
}

/// The caller's position in one fund: live units × current NAV, with the cost basis for
/// P&L. A fund never subscribed to reports zero units at the seed NAV.
pub async fn get_position(positions: &dyn FundPositionReader, ledger: &dyn Ledger, nav: &dyn NavMarks, user: UserId, service: ServiceId) -> Result<PositionView, DomainError> {
	let cost_basis = positions.find(user, &service).await?.map(|p| p.cost_basis).unwrap_or(Usdt::ZERO);
	build_position_view(ledger, nav, user, service, cost_basis).await
}

/// All of the caller's fund positions holding units — free or committed to the book. A
/// holder whose every unit sits in a resting sell still has a position.
pub async fn list_positions(positions: &dyn FundPositionReader, ledger: &dyn Ledger, nav: &dyn NavMarks, user: UserId) -> Result<Vec<PositionView>, DomainError> {
	let mut out = Vec::new();
	for position in positions.list(user).await? {
		let view = build_position_view(ledger, nav, user, position.service, position.cost_basis).await?;
		if !view.units.is_zero() || !view.units_in_orders.is_zero() {
			out.push(view);
		}
	}
	Ok(out)
}

/// The current NAV + freshness for a fund (the seed NAV when never marked), plus the
/// supply headroom left against its allocation's cap, as `caller` may see it. Gated the
/// same way [`allocation_for_holder`] is: an unregistered service is `NotFound`, and
/// so — unless `unrestricted` or the caller holds its units — is one hidden from this
/// caller. A price is as good a probe as a title: were the NAV of a hidden product
/// readable, a locked slug would answer differently from an unregistered one and the
/// catalog could be enumerated through this route. `unrestricted` is the
/// `AllocationManage` view, gated at the boundary.
pub async fn fund_nav_view(
	allocations: &dyn AllocationRegistry,
	nav: &dyn NavMarks,
	ledger: &dyn Ledger,
	service: ServiceId,
	caller: UserId,
	unrestricted: bool,
	now_unix: i64,
) -> Result<FundNavView, DomainError> {
	let allocation = allocation_for_holder(allocations, ledger, &service, caller, unrestricted).await?.allocation;
	let balance = ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await?;
	let units_outstanding = Shares::from_base_units(balance.posted);
	let remaining_capacity = allocation.remaining_capacity(Shares::from_base_units(balance.posted.saturating_add(balance.pending)));
	let quote = nav_of(nav, ledger, &service).await?;
	Ok(FundNavView {
		service,
		nav: quote.nav,
		aum: quote.aum,
		units_outstanding,
		unit_cap: allocation.unit_cap(),
		remaining_capacity,
		posted_at: quote.posted_at_unix,
		stale: quote.is_stale(now_unix),
	})
}

/// The marks of `service` with `from ≤ posted_at ≤ to` (`0` = unbounded on that side;
/// `to` is clamped to `now`), oldest first, plus `caller`'s participation through the
/// same window. Visible to exactly whom [`fund_nav_view`] is: the history of a product
/// hidden from this caller is `NotFound`, as its price is. A reserved allocation has no
/// marks — its price is computed, never posted — so its history is its holder's flows
/// priced at the seed NAV: the chart's deep past, not its current price, which the
/// position card reads from [`nav_of`].
///
/// The participation is walked BACKWARDS from the live holding: the units at instant `t`
/// are today's units (free plus escrowed on the book) less every recorded flow after
/// `t`. Anchoring on the ledger rather than summing flows forward from zero means the
/// series always ends exactly where the position card is, whatever the history missed
/// — a channel that moved units without a control-plane record shows as a wrong deep
/// past, never as a jump at the end of the line.
///
/// Four ports and a six-field request; a struct to carry them would serve this one call
/// site only, so the lint is waived the way `AppState::new` waives it.
#[allow(clippy::too_many_arguments)]
pub async fn fund_nav_history(
	allocations: &dyn AllocationRegistry,
	nav: &dyn NavMarks,
	ledger: &dyn Ledger,
	positions: &dyn FundPositionReader,
	service: ServiceId,
	caller: UserId,
	unrestricted: bool,
	from_unix: i64,
	to_unix: i64,
	now_unix: i64,
) -> Result<FundNavHistoryView, DomainError> {
	allocation_for_holder(allocations, ledger, &service, caller, unrestricted).await?;
	let to_unix = if to_unix == 0 { now_unix } else { to_unix.min(now_unix) };
	if from_unix < 0 || from_unix > to_unix {
		return Err(DomainError::Validation("history window is empty — `from` must not be after `to`".into()));
	}
	let mut marks = nav.history(&service, from_unix, to_unix, MAX_NAV_HISTORY_MARKS + 1).await?;
	let truncated = marks.len() > MAX_NAV_HISTORY_MARKS;
	if truncated {
		marks.remove(0);
	}
	// The price in force when the window opens comes from the newest mark before it. With
	// no lower bound and no cap hit there is none: before a fund's first mark it trades at
	// the seed NAV. `anchor` falls back to the fund's EARLIEST mark when none is old enough;
	// that one is inside the window (or past it) and must not price the instants before it.
	let window_start = if from_unix > 0 {
		from_unix
	} else if truncated {
		marks[0].posted_at_unix
	} else {
		0
	};
	let pre_window = if window_start > 0 {
		nav.anchor(&service, window_start).await?.filter(|v| v.posted_at_unix <= window_start)
	} else {
		None
	};
	let free = ledger.balance(&LedgerAccountKey::UserShares(service.clone(), caller)).await?.posted;
	let escrowed = ledger.balance(&LedgerAccountKey::BookShares(service.clone(), caller)).await?.posted;
	let live_units = free.checked_add(escrowed).ok_or_else(|| DomainError::Validation("position units overflow".into()))?;
	let flows = positions.unit_flows(caller, &service).await?;
	let participation = participation_series(live_units, &flows, pre_window.as_ref(), &marks, from_unix, to_unix)?;
	Ok(FundNavHistoryView {
		service,
		marks,
		participation,
		truncated,
	})
}

/// The participation series over `[from, to]`: one point at the window's start (when it
/// has one), at every mark, at every unit flow inside it, and at `to` — each valued at
/// the units held after everything up to that second × the NAV in force then (the seed
/// NAV before the first mark; `pre_window` is the mark in force when the window opens).
/// Instants before the caller's first flow are left out — a fund's marks are not this
/// holder's history until they hold something. A caller with no flows and no units has
/// no series at all.
fn participation_series(live_units: u128, flows: &[UnitFlow], pre_window: Option<&Valuation>, marks: &[Valuation], from: i64, to: i64) -> Result<Vec<ParticipationPoint>, DomainError> {
	if flows.is_empty() && live_units == 0 {
		return Ok(Vec::new());
	}
	// `after[i]` = the net units that arrived strictly after `flows[i - 1]`, i.e. the sum
	// of `flows[i..]`; `units_at(t)` subtracts the flows later than `t` from the live count.
	let mut after = vec![0i128; flows.len() + 1];
	for (i, flow) in flows.iter().enumerate().rev() {
		after[i] = after[i + 1].saturating_add(flow.delta);
	}
	let units_at = |t: i64| -> Shares {
		let i = flows.partition_point(|f| f.at_unix <= t);
		let units = i128::try_from(live_units).unwrap_or(i128::MAX).saturating_sub(after[i]);
		Shares::from_base_units(u128::try_from(units).unwrap_or(0))
	};
	let price_at = |t: i64| -> Nav {
		let i = marks.partition_point(|m| m.posted_at_unix <= t);
		match i.checked_sub(1) {
			Some(last) => marks[last].nav,
			None => pre_window.filter(|v| v.posted_at_unix <= t).map_or(Nav::SEED, |v| v.nav),
		}
	};
	let first_flow = flows.first().map_or(to, |f| f.at_unix);
	let mut instants: Vec<i64> = Vec::with_capacity(marks.len() + flows.len() + 2);
	if from > 0 {
		instants.push(from);
	}
	instants.extend(marks.iter().map(|m| m.posted_at_unix));
	instants.extend(flows.iter().map(|f| f.at_unix).filter(|&at| at >= from && at <= to));
	instants.push(to);
	instants.retain(|&t| t >= first_flow);
	instants.sort_unstable();
	instants.dedup();
	instants
		.into_iter()
		.map(|t| {
			Ok(ParticipationPoint {
				at_unix: t,
				value: price_at(t).value(units_at(t))?,
			})
		})
		.collect()
}

/// Operator posts a fund's total AUM; NAV is derived (`AUM / units_outstanding`, read
/// live from TigerBeetle). Rejects zero units (NAV undefined) and a move beyond
/// [`MAX_NAV_MOVE_PCT`] measured against BOTH the previous mark and the mark anchoring
/// the [`NAV_MOVE_WINDOW_SECS`] window. There is no way past the guard on this path: a
/// larger move is proposed to the owners and recorded by the consilium's execution
/// through the same [`record_valuation`] writer. Records the mark (with `posted_by`) and
/// returns it.
///
/// Gated on the allocation *existing* (any state — a closed product still gets marked so
/// queued redemptions price correctly). Without this an AUM post would write a valuation
/// history for a service no registry entry backs — the second way a phantom fund used to
/// come into being. A reserved allocation refuses a mark outright: its price is computed
/// from its holdings ([`nav_of`]), and a posted figure would be one nothing reads.
pub async fn post_fund_valuation(
	allocations: &dyn AllocationRegistry,
	nav: &dyn NavMarks,
	ledger: &dyn Ledger,
	service: ServiceId,
	aum: Usdt,
	posted_by: &str,
	now_unix: i64,
) -> Result<Valuation, DomainError> {
	refuse_mark_on_reserved(&service)?;
	allocations_app::get(allocations, &service).await?;
	let derived = Nav::from_aum(aum, issued_supply(ledger, &service).await?)?;
	if let Some(prev) = nav.current(&service).await? {
		if nav_move_exceeds(prev.nav, derived, MAX_NAV_MOVE_PCT) {
			return Err(move_guard_tripped(prev.nav, derived, "the previous mark"));
		}
		// A fund with a current mark always has an anchor (at worst its first mark), so a
		// missing one is not a pass — it is the same "never marked" case `prev` already
		// excluded, and the guard simply has nothing older to measure against.
		if let Some(anchor) = nav.anchor(&service, now_unix.saturating_sub(NAV_MOVE_WINDOW_SECS)).await?
			&& nav_move_exceeds(anchor.nav, derived, MAX_NAV_MOVE_PCT)
		{
			return Err(move_guard_tripped(anchor.nav, derived, "the mark anchoring the rolling window"));
		}
	}
	record_valuation(nav, ledger, ValuationId::new(), service, aum, posted_by).await
}

/// The one writer of a valuation mark, shared by the cap-checked direct post and the
/// owners' override (`consilium::execute`): derive NAV from the LIVE unit supply, append
/// the mark, return it. Applies NO move guard — every caller decides its own admission
/// before reaching this. The allocation gate is the caller's too: the direct post checks it
/// per request, the override checked it at open.
pub async fn record_valuation(nav: &dyn NavMarks, ledger: &dyn Ledger, id: ValuationId, service: ServiceId, aum: Usdt, posted_by: &str) -> Result<Valuation, DomainError> {
	let units = issued_supply(ledger, &service).await?;
	// `from_aum` rejects zero units — NAV is undefined with nothing outstanding.
	let derived = Nav::from_aum(aum, units)?;
	let posted_at_unix = nav.record(id, &service, aum, units, derived, posted_by).await?;
	Ok(Valuation {
		service,
		aum,
		units_outstanding: units,
		nav: derived,
		posted_by: posted_by.to_owned(),
		posted_at_unix,
	})
}

/// A reserved allocation (`fee`, `fund`) takes no mark: its NAV is the value of what it
/// holds, computed by [`nav_of`] on every read. Shared by the direct post and the owners'
/// override, which are the two ways a mark can be asked for.
pub fn refuse_mark_on_reserved(service: &ServiceId) -> Result<(), DomainError> {
	if service.is_reserved() {
		return Err(DomainError::Validation(format!(
			"'{service}' is a reserved allocation: its price is computed from what it holds and cannot be marked"
		)));
	}
	Ok(())
}

/// The settled supply NAV is derived against — posted units only, unlike
/// [`issued_units`], which also counts in-flight mints for the capacity gate.
async fn issued_supply(ledger: &dyn Ledger, service: &ServiceId) -> Result<Shares, DomainError> {
	Ok(Shares::from_base_units(ledger.balance(&LedgerAccountKey::SharesOutstanding(service.clone())).await?.posted))
}

fn move_guard_tripped(from: Nav, to: Nav, against: &str) -> DomainError {
	DomainError::Validation(format!(
		"nav move {from} → {to} exceeds {MAX_NAV_MOVE_PCT}% against {against} — open a valuation-override consilium for the owners to approve it"
	))
}
/// Assemble a position view: read the live unit balances — the holding and the book
/// escrow — and the current NAV, value the two together.
async fn build_position_view(ledger: &dyn Ledger, nav: &dyn NavMarks, user: UserId, service: ServiceId, cost_basis: Usdt) -> Result<PositionView, DomainError> {
	let units = Shares::from_base_units(ledger.balance(&LedgerAccountKey::UserShares(service.clone(), user)).await?.posted);
	let units_in_orders = Shares::from_base_units(ledger.balance(&LedgerAccountKey::BookShares(service.clone(), user)).await?.posted);
	let quote = nav_of(nav, ledger, &service).await?;
	let owned = units.checked_add(units_in_orders).ok_or_else(|| DomainError::Validation("position units overflow".into()))?;
	let value = quote.nav.value(owned)?;
	Ok(PositionView {
		service,
		units,
		units_in_orders,
		nav: quote.nav,
		value,
		cost_basis,
		nav_as_of: quote.posted_at_unix,
	})
}

/// `|new − prev| / prev > pct%`, computed on base units (saturating; a previous NAV of
/// zero makes any non-zero move "exceed", so recovering a wiped-out fund is an owners'
/// decision — a valuation-override consilium — never a single post).
fn nav_move_exceeds(prev: Nav, new: Nav, pct: u128) -> bool {
	let (p, n) = (prev.base_units(), new.base_units());
	p.abs_diff(n).saturating_mul(100) > p.saturating_mul(pct)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn nav_move_guard_trips_past_threshold() {
		let one = Nav::parse_decimal("1").unwrap();
		// +49% is fine, +51% trips, at the 50% threshold.
		assert!(!nav_move_exceeds(one, Nav::parse_decimal("1.49").unwrap(), MAX_NAV_MOVE_PCT));
		assert!(nav_move_exceeds(one, Nav::parse_decimal("1.51").unwrap(), MAX_NAV_MOVE_PCT));
		// A 10x fat-finger trips hard; a drop to zero trips; recovery from zero always trips.
		assert!(nav_move_exceeds(one, Nav::parse_decimal("10").unwrap(), MAX_NAV_MOVE_PCT));
		assert!(nav_move_exceeds(one, Nav::parse_decimal("0").unwrap(), MAX_NAV_MOVE_PCT));
		assert!(nav_move_exceeds(Nav::parse_decimal("0").unwrap(), one, MAX_NAV_MOVE_PCT));
	}

	fn mark(at: i64, nav: &str) -> Valuation {
		Valuation {
			service: ServiceId::parse("svc").unwrap(),
			aum: Usdt::ZERO,
			units_outstanding: Shares::from_base_units(1),
			nav: Nav::parse_decimal(nav).unwrap(),
			posted_by: "op".into(),
			posted_at_unix: at,
		}
	}

	fn flow(at: i64, units: &str) -> UnitFlow {
		let (sign, digits) = units.strip_prefix('-').map_or((1, units), |d| (-1, d));
		UnitFlow {
			at_unix: at,
			delta: sign * i128::try_from(Shares::parse_decimal(digits).unwrap().base_units()).unwrap(),
		}
	}

	fn points(series: &[ParticipationPoint]) -> Vec<(i64, String)> {
		series.iter().map(|p| (p.at_unix, p.value.to_decimal_string())).collect()
	}

	#[test]
	fn participation_walks_back_from_the_live_holding_and_steps_at_marks_and_flows() {
		// Bought 100 at seed, marked to 1.5, bought 50 more, marked to 2, sold 30 on the
		// book: 120 units live. The series is anchored on that 120, so every earlier
		// point is 120 less what arrived after it.
		let flows = [flow(10, "100"), flow(30, "50"), flow(50, "-30")];
		let marks = [mark(20, "1.5"), mark(40, "2")];
		let live = Shares::parse_decimal("120").unwrap().base_units();
		let series = participation_series(live, &flows, None, &marks, 0, 60).unwrap();
		assert_eq!(
			points(&series),
			vec![
				(10, "100".to_string()), // 100 units at the seed NAV
				(20, "150".to_string()), // marked to 1.5
				(30, "225".to_string()), // +50 units at 1.5
				(40, "300".to_string()), // marked to 2
				(50, "240".to_string()), // −30 units at 2
				(60, "240".to_string()), // "now": the live holding at the current mark
			]
		);
	}

	#[test]
	fn participation_window_opens_on_the_pre_window_price_and_the_units_held_then() {
		let flows = [flow(10, "100"), flow(30, "50")];
		let in_window = [mark(40, "2")];
		let before = mark(20, "1.5");
		let live = Shares::parse_decimal("150").unwrap().base_units();
		let series = participation_series(live, &flows, Some(&before), &in_window, 25, 60).unwrap();
		// Opens at `from` with the 100 units held then, priced at the mark before the window.
		assert_eq!(
			points(&series),
			vec![(25, "150".to_string()), (30, "225".to_string()), (40, "300".to_string()), (60, "300".to_string())]
		);
	}

	#[test]
	fn participation_ignores_marks_before_the_holder_arrived_and_is_empty_for_a_stranger() {
		let marks = [mark(5, "1.2"), mark(20, "1.5")];
		assert!(participation_series(0, &[], None, &marks, 0, 60).unwrap().is_empty());
		let flows = [flow(10, "100")];
		let live = Shares::parse_decimal("100").unwrap().base_units();
		let series = participation_series(live, &flows, None, &marks, 0, 60).unwrap();
		assert_eq!(points(&series), vec![(10, "120".to_string()), (20, "150".to_string()), (60, "150".to_string())]);
	}

	#[test]
	fn participation_never_goes_negative_when_the_history_is_missing_a_channel() {
		// Live says 10, the records say 40 arrived after t=10: the past clamps at zero
		// rather than underflowing — the wrong deep past the doc promises, not a panic.
		let flows = [flow(10, "5"), flow(30, "40")];
		let live = Shares::parse_decimal("10").unwrap().base_units();
		let series = participation_series(live, &flows, None, &[], 0, 60).unwrap();
		assert_eq!(points(&series), vec![(10, "0".to_string()), (30, "10".to_string()), (60, "10".to_string())]);
	}
}
