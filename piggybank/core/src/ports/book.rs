//! Persistence + read port for the allocation book — orders, trades, the per-product
//! policy and the revision counter the live feed is keyed on.
//!
//! One port rather than a repository per aggregate, because the book's write is not a
//! transition on one aggregate: a placement touches the incoming order, every resting
//! order it hits and a trade between each pair, and the whole set has to commit under
//! the book's single write lock or not at all. [`BookStore::place`] is that transaction.
//! It takes the [`MatchingEngine`] as a parameter — the engine is pure and the adapter
//! merely runs it under the lock and records what it decided — so the matching rule can
//! change without the persistence changing with it.
//!
//! The escrow an order commits and the settlement of a fill are NOT written here: they
//! are [`BookEvent`](domain::book::BookEvent)s drained to the outbox inside the same
//! transaction, and the relay moves the money afterwards (Write-Last), exactly as a
//! subscription's cash and mint follow its row. So an order is on the book the moment
//! its row commits, and the balances a client reads catch up at the relay's latency.

use async_trait::async_trait;
use domain::{
	balance::ServiceId,
	book::{BookPolicy, CandleResolution, ClientOrderId, MatchingEngine, Order, OrderId, Price, Rejection, Side, Trade},
	error::DomainError,
	money::{Nav, Shares, Usdt},
	users::UserId,
};

use crate::ports::ledger::SpendGuard;

#[async_trait]
pub trait BookStore: Send + Sync {
	/// The allocation's trading terms, or `None` when no operator has set any — which
	/// the use case reads as [`BookPolicy::default`], a closed book.
	async fn policy(&self, service: &ServiceId) -> Result<Option<BookPolicyRecord>, DomainError>;

	/// Replace the allocation's terms (upsert). `NotFound` if the allocation is not
	/// registered — the row references it.
	async fn set_policy(&self, service: &ServiceId, policy: &BookPolicy) -> Result<BookPolicyRecord, DomainError>;

	/// The caller's order recorded under `client_order_id`, if any — the idempotency
	/// read the use case runs before minting a new id.
	async fn find_by_client_id(&self, user: UserId, client_order_id: &ClientOrderId) -> Result<Option<OrderRecord>, DomainError>;

	/// One order by id, whoever placed it. The use case decides what a caller may see.
	async fn find_order(&self, id: OrderId) -> Result<Option<OrderRecord>, DomainError>;

	/// The best resting price on `side` of the book (highest bid, or lowest ask), if
	/// any — what a market order is priced from.
	async fn best_price(&self, service: &ServiceId, side: Side) -> Result<Option<Price>, DomainError>;

	/// The placement transaction: under the book's write lock, read the opposite side
	/// best first, run `engine`, and record what it decided — the order (filled as far
	/// as it went, resting or cancelled per its time in force), a trade per fill, every
	/// maker's new state, the bumped revision — and drain the ledger facts to the outbox
	/// in the order the relay must apply them. `nav` is stamped on each trade for the
	/// buyer's high-water mark.
	///
	/// A rejection by the engine writes nothing. A `client_order_id` the caller already
	/// used (a race two concurrent retries lose to each other) writes nothing either and
	/// answers with the standing row. `guard` runs under the order's lock — for a buy the
	/// user's claim lock — before anything is written.
	async fn place(&self, order: Order, engine: &dyn MatchingEngine, policy: &BookPolicy, nav: Nav, guard: &dyn SpendGuard) -> Result<PlaceOutcome, DomainError>;

	/// Take a resting order off the book under the book's write lock, bumping the
	/// revision and draining the release of its unspent escrow. Idempotent on an order
	/// already cancelled (the book's current revision rides back unchanged); `Conflict`
	/// on a filled or rejected one; `NotFound` if unknown.
	async fn cancel(&self, id: OrderId) -> Result<CancelOutcome, DomainError>;

	/// The caller's resting orders, oldest first, optionally on one allocation.
	async fn list_open(&self, user: UserId, service: Option<&ServiceId>) -> Result<Vec<OrderRecord>, DomainError>;

	/// The caller's orders in every state, newest first, at most `limit`.
	async fn list_history(&self, user: UserId, service: Option<&ServiceId>, limit: u32) -> Result<Vec<OrderRecord>, DomainError>;

	/// The caller's own fills from either side, newest first, at most `limit`.
	async fn list_user_trades(&self, user: UserId, service: Option<&ServiceId>, limit: u32) -> Result<Vec<UserTrade>, DomainError>;

	/// The aggregated book: `depth` levels a side plus the tape's last trade and the
	/// trailing-day figures.
	async fn depth(&self, service: &ServiceId, depth: u32) -> Result<BookDepth, DomainError>;

	/// The public tape, newest first, at most `limit`.
	async fn list_trades(&self, service: &ServiceId, limit: u32) -> Result<Vec<TradeRecord>, DomainError>;

	/// OHLCV buckets of `resolution` over `[from, to)` unix seconds, oldest first, empty
	/// buckets omitted.
	async fn candles(&self, service: &ServiceId, resolution: CandleResolution, from: i64, to: i64) -> Result<Vec<Candle>, DomainError>;

	/// The book revision at which `user`'s orders on `service` last changed (0 = never).
	async fn orders_revision(&self, user: UserId, service: &ServiceId) -> Result<u64, DomainError>;
}

/// The terms as stored, with the DB stamp the domain does not model.
#[derive(Debug)]
pub struct BookPolicyRecord {
	pub service: ServiceId,
	pub policy: BookPolicy,
	/// Unix seconds of the last write.
	pub updated_at: i64,
}

/// An order as stored — the aggregate plus the DB-stamped timestamps.
#[derive(Clone, Debug)]
pub struct OrderRecord {
	pub order: Order,
	pub created_at: i64,
	pub updated_at: i64,
}

/// A trade as stored.
#[derive(Clone, Debug)]
pub struct TradeRecord {
	pub trade: Trade,
	/// Unix seconds the fill was recorded.
	pub executed_at: i64,
}

/// A trade from one party's point of view: which side they were on, which of their
/// orders it filled, and what they paid on it (zero as the maker).
#[derive(Clone, Debug)]
pub struct UserTrade {
	pub trade: TradeRecord,
	pub side: Side,
	pub order_id: OrderId,
	pub fee: Usdt,
}

/// What [`BookStore::place`] did.
#[derive(Debug)]
pub enum PlaceOutcome {
	/// The order was recorded; `trades` are its fills in the order they happened.
	Placed { order: OrderRecord, trades: Vec<TradeRecord>, revision: u64 },
	/// The engine refused the whole order; nothing was written.
	Rejected(Rejection),
	/// The client order id was already taken — the standing row, untouched.
	Existing(OrderRecord),
}

/// What [`BookStore::cancel`] left: the order as it now stands, and the book revision a
/// watching client should refetch at.
#[derive(Debug)]
pub struct CancelOutcome {
	pub order: OrderRecord,
	pub revision: u64,
}

/// One aggregated price level.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookLevel {
	pub price: Price,
	pub size: Shares,
	pub orders: u32,
}

/// The aggregated book. `bids` are best (highest) first, `asks` best (lowest) first.
#[derive(Clone, Debug)]
pub struct BookDepth {
	pub revision: u64,
	pub bids: Vec<BookLevel>,
	pub asks: Vec<BookLevel>,
	/// The last trade's price and the side that took it.
	pub last: Option<(Price, Side)>,
	/// The last trade at or before 24h ago — or, failing that, the first inside the
	/// window — against which the 24h change is measured.
	pub reference_24h: Option<Price>,
	pub volume_24h: Shares,
}

/// One OHLCV bucket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candle {
	/// Bucket start, unix seconds.
	pub time: i64,
	pub open: Price,
	pub high: Price,
	pub low: Price,
	pub close: Price,
	pub volume: Shares,
}
