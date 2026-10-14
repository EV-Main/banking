//! Book use cases — holders trading an allocation's units with each other.
//!
//! Shaped like [`subscribe`](super::funds::subscribe): the registry gate first, the
//! terms next, then a Read-First on the balance the order will escrow, and only then the
//! write — after which the relay is nudged to move the escrow and the live feed is told
//! the book moved. Two things differ from a subscription, both deliberate:
//!
//! - **The gate ignores `state`.** A closed product's holders may still trade among
//!   themselves — a book closes only by its own policy (`book_open`), never by the
//!   product no longer taking new money from the fund. Access still applies in full: a
//!   caller below `invest` may not trade, and a `hidden` product is `NotFound` to them.
//! - **NAV is not dealt at, so its staleness is not a gate.** The two parties set the
//!   price. The mark is still read (unguarded) and stamped on each trade, because the
//!   buyer's high-water mark blends it — performance is measured against NAV, so the mark
//!   must be one — and the snapshot shows it beside the quote.
//!
//! The matcher is a port too ([`MatchingEngine`]): the use case hands the store the
//! engine it was wired with, so swapping the matching rule is a composition-root change.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

use domain::{
	allocations::Allocation,
	balance::{LedgerAccountKey, ServiceId},
	book::{BookPolicy, CandleResolution, ClientOrderId, Locked, MatchingEngine, Order, OrderId, OrderKind, Price, Rejection, Side, Tif},
	error::DomainError,
	money::{Nav, Shares, Usdt},
	users::UserId,
};
use tokio::sync::{Notify, watch};

use crate::{
	application::{allocations as allocations_app, funds as funds_app},
	ports::{
		allocations::AllocationRegistry,
		book::{BookLevel, BookPolicyRecord, BookStore, Candle, OrderRecord, PlaceOutcome, TradeRecord, UserTrade},
		ledger::Ledger,
		nav::NavMarks,
		outflow::OutflowPolicy,
	},
};

/// The most buckets one candle request may span. A chart asks for a window, not an
/// export; a wider one is refused rather than aggregated unbounded.
pub const MAX_CANDLE_BUCKETS: i64 = 5_000;

/// The driven ports a book command borrows — the [`FundPorts`](super::funds::FundPorts)
/// idea, with the store, the engine and the live feed beside the ledger and the marks.
pub struct BookPorts<'a> {
	pub allocations: &'a dyn AllocationRegistry,
	pub ledger: &'a dyn Ledger,
	pub nav: &'a dyn NavMarks,
	pub store: &'a dyn BookStore,
	pub engine: &'a dyn MatchingEngine,
	/// The kill-switch and the caller's standing — the same facts a payout clears, read
	/// here so a pause stops new orders at the use case and not only at the boundary.
	pub outflow: &'a dyn OutflowPolicy,
	/// Nudged after the commit so the relay moves the escrow promptly.
	pub relay: &'a Notify,
	/// Told the revision after the commit so every `WatchBook` subscriber frames.
	pub feed: &'a BookFeed,
}

/// One order as the caller asked for it. `price` is required for a limit order and must
/// be absent for a market one, whose limit the hub derives.
pub struct PlaceOrderRequest {
	pub service: ServiceId,
	pub side: Side,
	pub kind: OrderKind,
	pub tif: Tif,
	pub price: Option<Price>,
	pub size: Shares,
	pub client_order_id: ClientOrderId,
}

/// The book as a client renders it — the depth plus the derived figures.
pub struct BookSnapshotView {
	pub service: ServiceId,
	pub revision: u64,
	pub bids: Vec<BookLevel>,
	pub asks: Vec<BookLevel>,
	pub last: Option<(Price, Side)>,
	pub mid: Option<Price>,
	pub spread: Option<Price>,
	pub nav: Nav,
	pub volume_24h: Shares,
	/// Signed percent, two decimals, e.g. `-2.50`; `None` without a reference trade.
	pub change_24h_pct: Option<String>,
	pub as_of: i64,
}

/// One `WatchBook` frame: the snapshot, the freshest public trades, and the revision at
/// which the watching caller's own orders last moved.
pub struct WatchFrame {
	pub snapshot: BookSnapshotView,
	pub trades: Vec<TradeRecord>,
	pub orders_revision: u64,
}

/// The in-process fan-out behind `WatchBook`: one `watch` channel per book, carrying
/// the latest revision. A `watch` rather than a `broadcast` because a subscriber that
/// falls behind must see the LATEST state, not every intermediate one — frames coalesce
/// by construction and a slow socket can never build a backlog. In-process only: the
/// hub is a single instance behind the relay's own singleton lock, so there is nothing
/// to fan out across, and Postgres `LISTEN`/`NOTIFY` would be a second bus for one node.
#[derive(Default)]
pub struct BookFeed {
	channels: Mutex<HashMap<String, watch::Sender<u64>>>,
}

impl BookFeed {
	pub fn new() -> Arc<Self> {
		Arc::new(Self::default())
	}

	/// Announce that `service`'s book is now at `revision`.
	pub fn publish(&self, service: &ServiceId, revision: u64) {
		// A poisoned lock means a publisher panicked mid-insert; the map is still valid
		// (an insert either happened or did not), so recovering is right.
		let mut channels = self.channels.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		channels.entry(service.as_str().to_owned()).or_insert_with(|| watch::channel(0).0).send_replace(revision);
	}

	/// A receiver already primed to fire once, so a subscriber's first frame comes at
	/// once rather than on the next change.
	pub fn subscribe(&self, service: &ServiceId) -> watch::Receiver<u64> {
		let mut channels = self.channels.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		let mut receiver = channels.entry(service.as_str().to_owned()).or_insert_with(|| watch::channel(0).0).subscribe();
		receiver.mark_changed();
		receiver
	}
}

/// Resolve `service` as `user` and assert they may trade its units. `NotFound` for an
/// unregistered or hidden product; [`DomainError::Precondition`] for a caller below
/// `invest`. The lifecycle `state` is deliberately not consulted (see the module doc).
pub async fn require_tradable(allocations: &dyn AllocationRegistry, service: &ServiceId, user: UserId) -> Result<Allocation, DomainError> {
	let record = allocations_app::get_for(allocations, service, user, false).await?;
	if !record.caller_access.permits_investing() {
		return Err(DomainError::Precondition(format!(
			"allocation '{}' is not open to you for trading — an operator must grant you 'invest' access",
			service
		)));
	}
	Ok(record.allocation)
}

/// Resolve `service` for a read as `caller` sees it: `NotFound` when hidden from them,
/// unless `unrestricted` (an `AllocationManage` holder). What every book read gates on,
/// so the book of a product a caller may not see cannot be probed.
pub async fn require_visible(allocations: &dyn AllocationRegistry, service: &ServiceId, caller: UserId, unrestricted: bool) -> Result<(), DomainError> {
	allocations_app::get_for(allocations, service, caller, unrestricted).await.map(|_| ())
}

/// The terms `service` trades under: what the operator set, or the defaults (a closed
/// book) when they set nothing.
pub async fn policy(store: &dyn BookStore, service: &ServiceId) -> Result<BookPolicyRecord, DomainError> {
	Ok(store.policy(service).await?.unwrap_or_else(|| BookPolicyRecord {
		service: service.clone(),
		policy: BookPolicy::default(),
		updated_at: 0,
	}))
}

/// Replace the terms (`AllocationManage` at the boundary). The allocation must be
/// registered, in any state. Opening the book on an `in_kind` product needs the
/// operator's acknowledgement (`allow_unbacked_trading`); refused as a `Precondition`
/// with nothing written, so the terms in force stay whatever they were. The same gate
/// runs again on every placement ([`BookPolicy::ensure_tradable_for`]) — this one is the
/// early, legible refusal at the console; that one is the backstop.
pub async fn set_policy(allocations: &dyn AllocationRegistry, store: &dyn BookStore, service: &ServiceId, policy: BookPolicy) -> Result<BookPolicyRecord, DomainError> {
	let allocation = allocations_app::get(allocations, service).await?;
	if policy.book_open() {
		policy.ensure_tradable_for(service, allocation.backing())?;
	}
	store.set_policy(service, &policy).await
}

/// The fund's current price, staleness ignored — the book does not deal at it, it only
/// records it (on each trade, for the buyer's high-water mark) and shows it.
async fn current_nav(nav: &dyn NavMarks, ledger: &dyn Ledger, service: &ServiceId) -> Result<Nav, DomainError> {
	Ok(funds_app::nav_of(nav, ledger, service).await?.nav)
}

/// Place an order for `user`. Gates in order: the registry (registered, visible, and
/// `invest` for this caller), the policy (`book_open`, and the unbacked-trading
/// acknowledgement when the units are held in kind), the grid (tick and lot), the
/// pricing of a market order off the best opposite quote, the idempotency read, and the
/// Read-First on what the order escrows — a sell needs the units free in the holding, a
/// buy the worst-case cash (notional at the limit plus the taker fee) free in the claim.
/// TigerBeetle's non-negative flags are the backstop behind that optimistic read; a
/// raced over-spend parks the lock and the relay marks the order `rejected`.
///
/// Idempotent by `client_order_id`: a repeat returns the order the id already names,
/// provided it asks for the same thing; the same id for a different order is a
/// [`DomainError::Conflict`].
pub async fn place_order(ports: &BookPorts<'_>, user: UserId, request: PlaceOrderRequest) -> Result<OrderRecord, DomainError> {
	// The read-only kill-switch pauses every user-initiated money mutation, and an order
	// escrows money the moment it is recorded; a frozen or disabled owner may cancel but
	// not place. Checked first, before any read that costs a round trip to the ledger.
	if ports.outflow.outflows_paused().await? {
		return Err(DomainError::Precondition("money movements are temporarily paused (read-only mode)".into()));
	}
	if ports.outflow.standing(user).await?.is_some_and(|standing| standing.blocked) {
		return Err(DomainError::Forbidden("account is frozen".into()));
	}
	let allocation = require_tradable(ports.allocations, &request.service, user).await?;
	let policy = policy(ports.store, &request.service).await?.policy;
	policy.ensure_tradable_for(&request.service, allocation.backing())?;
	let size = policy.size(request.size)?;
	let price = match request.kind {
		OrderKind::Limit => policy.price(request.price.ok_or_else(|| DomainError::Validation("a limit order needs a price".into()))?)?,
		OrderKind::Market => {
			if request.price.is_some() {
				return Err(DomainError::Validation("a market order takes no price — it is priced from the book".into()));
			}
			if request.tif != Tif::Ioc {
				return Err(DomainError::Validation("a market order is immediate-or-cancel".into()));
			}
			let best = ports
				.store
				.best_price(&request.service, request.side.opposite())
				.await?
				.ok_or_else(|| DomainError::Precondition(format!("no {} liquidity on the book for '{}'", request.side.opposite().as_str(), request.service)))?;
			policy.market_limit(request.side, best)?
		}
	};
	if let Some(existing) = ports.store.find_by_client_id(user, &request.client_order_id).await? {
		return same_request_or_conflict(existing, &request);
	}
	let reserved = match request.side {
		Side::Sell => {
			let holding = ports.ledger.balance(&LedgerAccountKey::UserShares(request.service.clone(), user)).await?;
			if Shares::from_base_units(holding.available()) < size.shares() {
				return Err(DomainError::Validation("insufficient units to sell".into()));
			}
			Locked::Units(size.shares())
		}
		Side::Buy => {
			let reserve = policy.buy_reserve(size.shares(), price)?;
			// Net of the user's compartment moves still in flight (`Ledger::spendable`).
			if Usdt::from_base_units(ports.ledger.spendable(&LedgerAccountKey::UserClaim(user)).await?) < reserve {
				return Err(DomainError::Validation("insufficient available balance to buy".into()));
			}
			Locked::Cash(reserve)
		}
	};
	let nav = current_nav(ports.nav, ports.ledger, &request.service).await?;
	let order = Order::place(
		OrderId::new(),
		request.service.clone(),
		user,
		request.client_order_id.clone(),
		request.side,
		request.kind,
		request.tif,
		price,
		size,
		reserved,
	);
	match ports.store.place(order, ports.engine, &policy, nav).await? {
		PlaceOutcome::Placed { order, revision, .. } => {
			ports.relay.notify_one();
			ports.feed.publish(&request.service, revision);
			Ok(order)
		}
		PlaceOutcome::Rejected(rejection) => Err(match rejection {
			Rejection::SelfTrade | Rejection::WouldCross => DomainError::Validation(rejection.reason().to_owned()),
		}),
		PlaceOutcome::Existing(existing) => same_request_or_conflict(existing, &request),
	}
}

fn same_request_or_conflict(existing: OrderRecord, request: &PlaceOrderRequest) -> Result<OrderRecord, DomainError> {
	if existing.order.matches_request(&request.service, request.side, request.kind, request.tif, request.size) {
		Ok(existing)
	} else {
		Err(DomainError::Conflict(format!(
			"client order id '{}' already names a different order",
			request.client_order_id.as_str()
		)))
	}
}

/// Cancel the caller's own resting order; the relay hands its unspent escrow back.
/// Ownership is checked here; the aggregate refuses to cancel a filled one.
pub async fn cancel_order(store: &dyn BookStore, relay: &Notify, feed: &BookFeed, id: OrderId, user: UserId) -> Result<OrderRecord, DomainError> {
	let existing = store.find_order(id).await?.ok_or_else(|| DomainError::NotFound {
		entity: "order",
		id: id.to_string(),
	})?;
	if existing.order.user() != user {
		return Err(DomainError::Forbidden("not your order".into()));
	}
	let outcome = store.cancel(id).await?;
	relay.notify_one();
	feed.publish(existing.order.service(), outcome.revision);
	Ok(outcome.order)
}

pub async fn list_open_orders(store: &dyn BookStore, user: UserId, service: Option<&ServiceId>) -> Result<Vec<OrderRecord>, DomainError> {
	store.list_open(user, service).await
}

pub async fn list_order_history(store: &dyn BookStore, user: UserId, service: Option<&ServiceId>, limit: u32) -> Result<Vec<OrderRecord>, DomainError> {
	store.list_history(user, service, limit).await
}

pub async fn list_user_trades(store: &dyn BookStore, user: UserId, service: Option<&ServiceId>, limit: u32) -> Result<Vec<UserTrade>, DomainError> {
	store.list_user_trades(user, service, limit).await
}

pub async fn list_trades(store: &dyn BookStore, service: &ServiceId, limit: u32) -> Result<Vec<TradeRecord>, DomainError> {
	store.list_trades(service, limit).await
}

/// The aggregated book with its derived figures. Visibility is the caller's to check
/// first ([`require_visible`]).
pub async fn snapshot(store: &dyn BookStore, nav: &dyn NavMarks, ledger: &dyn Ledger, service: &ServiceId, depth: u32, now_unix: i64) -> Result<BookSnapshotView, DomainError> {
	let depth_view = store.depth(service, depth).await?;
	let nav = current_nav(nav, ledger, service).await?;
	let best_bid = depth_view.bids.first().map(|level| level.price);
	let best_ask = depth_view.asks.first().map(|level| level.price);
	let (mid, spread) = match (best_bid, best_ask) {
		(Some(bid), Some(ask)) => (
			Some(Price::from_base_units(
				bid.base_units() / 2 + ask.base_units() / 2 + (bid.base_units() % 2 + ask.base_units() % 2) / 2,
			)),
			Some(Price::from_base_units(ask.base_units().saturating_sub(bid.base_units()))),
		),
		_ => (None, None),
	};
	let change_24h_pct = match (depth_view.last, depth_view.reference_24h) {
		(Some((last, _)), Some(reference)) => percent_change(reference, last),
		_ => None,
	};
	Ok(BookSnapshotView {
		service: service.clone(),
		revision: depth_view.revision,
		bids: depth_view.bids,
		asks: depth_view.asks,
		last: depth_view.last,
		mid,
		spread,
		nav,
		volume_24h: depth_view.volume_24h,
		change_24h_pct,
		as_of: now_unix,
	})
}

/// One frame for a `WatchBook` subscriber: the snapshot, the freshest `trades` public
/// trades, and where the caller's own orders last moved.
// Three ports and a five-field request; a struct would serve this one call site only.
#[allow(clippy::too_many_arguments)]
pub async fn watch_frame(
	store: &dyn BookStore,
	nav: &dyn NavMarks,
	ledger: &dyn Ledger,
	service: &ServiceId,
	caller: UserId,
	depth: u32,
	trades: u32,
	now_unix: i64,
) -> Result<WatchFrame, DomainError> {
	let snapshot = snapshot(store, nav, ledger, service, depth, now_unix).await?;
	let trades = store.list_trades(service, trades).await?;
	let orders_revision = store.orders_revision(caller, service).await?;
	Ok(WatchFrame { snapshot, trades, orders_revision })
}

/// OHLCV buckets over `[from, to)`. The window is bounded by [`MAX_CANDLE_BUCKETS`] at
/// the requested resolution.
pub async fn candles(store: &dyn BookStore, service: &ServiceId, resolution: CandleResolution, from: i64, to: i64) -> Result<Vec<Candle>, DomainError> {
	if from < 0 || to <= from {
		return Err(DomainError::Validation("candle window must be non-empty and ordered".into()));
	}
	if (to - from) / resolution.seconds() > MAX_CANDLE_BUCKETS {
		return Err(DomainError::Validation(format!(
			"candle window spans more than {MAX_CANDLE_BUCKETS} buckets at {}",
			resolution.as_str()
		)));
	}
	store.candles(service, resolution, from, to).await
}

/// `(last − reference) / reference` as a signed percent with two decimals, floored
/// toward zero. `None` when the reference is zero (a percent of nothing).
fn percent_change(reference: Price, last: Price) -> Option<String> {
	let (reference, last) = (reference.base_units(), last.base_units());
	if reference == 0 {
		return None;
	}
	let (sign, diff) = if last >= reference { ("", last - reference) } else { ("-", reference - last) };
	// Hundredths of a percent: diff × 10 000 / reference, with the product widened
	// through `Price::scale` so a large base-unit price never overflows.
	let hundredths = Price::from_base_units(diff).scale(10_000, reference).ok()?.base_units();
	Some(format!("{sign}{}.{:02}", hundredths / 100, hundredths % 100))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_24h_change_is_a_signed_percent_with_two_decimals() {
		let price = |raw: &str| Price::parse_decimal(raw).unwrap();
		assert_eq!(percent_change(price("1"), price("1.025")).as_deref(), Some("2.50"));
		assert_eq!(percent_change(price("2"), price("1.5")).as_deref(), Some("-25.00"));
		assert_eq!(percent_change(price("1"), price("1")).as_deref(), Some("0.00"));
		// Floored toward zero: 1/3 of a percent is 0.33, not 0.34.
		assert_eq!(percent_change(price("3"), price("3.01")).as_deref(), Some("0.33"));
		assert_eq!(percent_change(Price::from_base_units(0), price("1")), None);
	}

	#[tokio::test]
	async fn a_subscriber_frames_at_once_and_then_only_on_change() {
		let feed = BookFeed::new();
		let service = ServiceId::parse("service_arb").unwrap();
		let mut receiver = feed.subscribe(&service);
		// Primed: the first wait returns immediately, at the revision the book is at.
		receiver.changed().await.unwrap();
		assert_eq!(*receiver.borrow_and_update(), 0);
		feed.publish(&service, 3);
		feed.publish(&service, 4);
		receiver.changed().await.unwrap();
		// Two publishes coalesce into one frame at the latest revision.
		assert_eq!(*receiver.borrow_and_update(), 4);
		assert!(!receiver.has_changed().unwrap(), "nothing new until the next publish");
	}
}
