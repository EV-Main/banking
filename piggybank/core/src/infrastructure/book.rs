//! Postgres adapter for the [`BookStore`] port.
//!
//! **One writer per book.** `place` and `cancel` open with `pg_advisory_xact_lock(
//! hashtext(service))`, held to commit, so every change to one allocation's book is
//! serialized and the resting side the matcher reads is the whole truth at that instant.
//! There is no in-memory book: the resting orders ARE the `book_orders` rows in a resting
//! state, sorted best-first by the partial index, and a depth snapshot is an aggregate
//! over them. That trades a little latency per placement for a book that survives a
//! restart, needs no warm-up and cannot drift from what is persisted.
//!
//! **The ledger facts are drained here, in order.** A placement writes its
//! [`BookEvent`]s to the outbox in the sequence the relay has to apply them — the
//! taker's escrow lock, then each fill, then the release of whatever reached a terminal
//! state — and the relay's strict `seq` order does the rest. The escrow is moved by the
//! relay AFTER this commit (Write-Last), so the order is on the book before its units or
//! cash have moved; the use case's Read-First is what keeps that honest, and TigerBeetle's
//! non-negative flags are the backstop when two placements race it (see
//! [`super::relay`], which marks such an order `rejected`).
//!
//! **Time priority is `seq`, not `created_at`.** `now()` is the transaction's start; two
//! transactions that queued on the advisory lock carry it in the order they BEGAN, not
//! the order they were admitted. The identity column is assigned at insert, under the
//! lock, and is what the matcher sorts on.

use async_trait::async_trait;
use domain::{
	balance::{Party, ServiceId},
	book::{
		BookEvent, BookPolicy, CancelReason, CandleResolution, ClientOrderId, IncomingOrder, Locked, MatchingEngine, Order, OrderId, OrderKind, OrderSnapshot, OrderState, Price,
		RestingOrder, Side, Tif, Trade, TradeId,
	},
	error::DomainError,
	money::{Nav, Shares, Usdt},
	users::UserId,
};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::{
	infrastructure::outbox,
	ports::{
		book::{BookDepth, BookLevel, BookPolicyRecord, BookStore, CancelOutcome, Candle, OrderRecord, PlaceOutcome, TradeRecord, UserTrade},
		ledger::SpendGuard,
	},
};

/// The `event_log.aggregate` names the book's facts are filed under — an order's lock and
/// release under the order, a fill under the trade.
const ORDER_AGGREGATE: &str = "book_order";
const TRADE_AGGREGATE: &str = "book_trade";

/// sqlx 0.9 accepts only `&'static str` SQL (its injection guardrail), so the shared
/// column list is spelled out per query rather than interpolated; the test at the bottom
/// holds every copy to these two.
#[cfg(test)]
const ORDER_COLUMNS: &str = "id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, fee_paid, reserved, state, reject_reason, cancel_reason, \
	 EXTRACT(EPOCH FROM created_at)::bigint AS created_at, EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at";
const SELECT_ORDER_BY_ID: &str = "SELECT id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, fee_paid, reserved, state, reject_reason, cancel_reason, \
	 EXTRACT(EPOCH FROM created_at)::bigint AS created_at, EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at \
	 FROM book_orders WHERE id = $1";
const SELECT_ORDER_BY_ID_FOR_UPDATE: &str = "SELECT id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, fee_paid, reserved, state, reject_reason, cancel_reason, \
	 EXTRACT(EPOCH FROM created_at)::bigint AS created_at, EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at \
	 FROM book_orders WHERE id = $1 FOR UPDATE";
const SELECT_ORDER_BY_CLIENT_ID: &str = "SELECT id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, fee_paid, reserved, state, reject_reason, cancel_reason, \
	 EXTRACT(EPOCH FROM created_at)::bigint AS created_at, EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at \
	 FROM book_orders WHERE user_id = $1 AND client_order_id = $2";
/// `$3` is the caller's limit: a buy needs only the asks at or below it, a sell only the
/// bids at or above it — the matcher stops at the first that does not cross anyway, so
/// the rows past it are never worth reading.
const SELECT_ASKS_CROSSING: &str = "SELECT id, user_id, price, size, filled FROM book_orders \
	 WHERE service = $1 AND side = 'sell' AND state IN ('open', 'partially_filled') AND price::numeric <= $2::numeric \
	 ORDER BY price::numeric ASC, seq ASC";
const SELECT_BIDS_CROSSING: &str = "SELECT id, user_id, price, size, filled FROM book_orders \
	 WHERE service = $1 AND side = 'buy' AND state IN ('open', 'partially_filled') AND price::numeric >= $2::numeric \
	 ORDER BY price::numeric DESC, seq ASC";
const SELECT_BEST_ASK: &str = "SELECT price FROM book_orders WHERE service = $1 AND side = 'sell' AND state IN ('open', 'partially_filled') ORDER BY price::numeric ASC, seq ASC LIMIT 1";
const SELECT_BEST_BID: &str = "SELECT price FROM book_orders WHERE service = $1 AND side = 'buy' AND state IN ('open', 'partially_filled') ORDER BY price::numeric DESC, seq ASC LIMIT 1";
const SELECT_ASK_LEVELS: &str = "SELECT price, SUM(size::numeric - filled::numeric)::text AS size, COUNT(*)::int AS orders FROM book_orders \
	 WHERE service = $1 AND side = 'sell' AND state IN ('open', 'partially_filled') GROUP BY price ORDER BY price::numeric ASC LIMIT $2";
const SELECT_BID_LEVELS: &str = "SELECT price, SUM(size::numeric - filled::numeric)::text AS size, COUNT(*)::int AS orders FROM book_orders \
	 WHERE service = $1 AND side = 'buy' AND state IN ('open', 'partially_filled') GROUP BY price ORDER BY price::numeric DESC LIMIT $2";
const SELECT_OPEN_ORDERS: &str = "SELECT id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, fee_paid, reserved, state, reject_reason, cancel_reason, \
	 EXTRACT(EPOCH FROM created_at)::bigint AS created_at, EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at \
	 FROM book_orders WHERE user_id = $1 AND ($2::text IS NULL OR service = $2) AND state IN ('open', 'partially_filled') ORDER BY seq ASC";
const SELECT_ORDER_HISTORY: &str = "SELECT id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, fee_paid, reserved, state, reject_reason, cancel_reason, \
	 EXTRACT(EPOCH FROM created_at)::bigint AS created_at, EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at \
	 FROM book_orders WHERE user_id = $1 AND ($2::text IS NULL OR service = $2) ORDER BY seq DESC LIMIT $3";
#[cfg(test)]
const TRADE_COLUMNS: &str = "id, service, buyer_id, seller_id, buy_order_id, sell_order_id, taker_side, price, size, notional, fee, EXTRACT(EPOCH FROM executed_at)::bigint AS executed_at";
const SELECT_USER_TRADES: &str = "SELECT id, service, buyer_id, seller_id, buy_order_id, sell_order_id, taker_side, price, size, notional, fee, EXTRACT(EPOCH FROM executed_at)::bigint AS executed_at \
	 FROM book_trades WHERE (buyer_id = $1 OR seller_id = $1) AND ($2::text IS NULL OR service = $2) ORDER BY seq DESC LIMIT $3";
const SELECT_TAPE: &str = "SELECT id, service, buyer_id, seller_id, buy_order_id, sell_order_id, taker_side, price, size, notional, fee, EXTRACT(EPOCH FROM executed_at)::bigint AS executed_at \
	 FROM book_trades WHERE service = $1 ORDER BY seq DESC LIMIT $2";
const SELECT_TRADE_BY_ID: &str = "SELECT id, service, buyer_id, seller_id, buy_order_id, sell_order_id, taker_side, price, size, notional, fee, EXTRACT(EPOCH FROM executed_at)::bigint AS executed_at \
	 FROM book_trades WHERE id = $1";
/// One row per non-empty bucket. `open`/`close` are the first/last price by arrival
/// (`seq`, which also orders the fills of one placement that share `executed_at`).
const SELECT_CANDLES: &str = "SELECT (floor(EXTRACT(EPOCH FROM executed_at) / $4) * $4)::bigint AS bucket, \
	 (array_agg(price ORDER BY seq ASC))[1] AS open, MAX(price::numeric)::text AS high, MIN(price::numeric)::text AS low, \
	 (array_agg(price ORDER BY seq DESC))[1] AS close, SUM(size::numeric)::text AS volume \
	 FROM book_trades WHERE service = $1 AND executed_at >= to_timestamp($2) AND executed_at < to_timestamp($3) \
	 GROUP BY bucket ORDER BY bucket ASC";

pub struct PgBook {
	pool: PgPool,
}

impl PgBook {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

#[derive(sqlx::FromRow)]
struct OrderRow {
	id: Uuid,
	service: String,
	user_id: Uuid,
	client_order_id: String,
	side: String,
	kind: String,
	tif: String,
	price: String,
	size: String,
	filled: String,
	notional_filled: String,
	fee_paid: String,
	reserved: String,
	state: String,
	reject_reason: Option<String>,
	cancel_reason: Option<String>,
	created_at: i64,
	updated_at: i64,
}

impl OrderRow {
	fn into_record(self) -> Result<OrderRecord, DomainError> {
		let side = Side::parse(&self.side)?;
		let reserved = parse_units(&self.reserved, "order reserve")?;
		let order = Order::rehydrate(OrderSnapshot {
			id: OrderId::from_raw(self.id),
			service: ServiceId::parse(&self.service)?,
			user: UserId::from_raw(self.user_id),
			client_order_id: ClientOrderId::parse(&self.client_order_id)?,
			side,
			kind: OrderKind::parse(&self.kind)?,
			tif: Tif::parse(&self.tif)?,
			price: Price::from_base_units(parse_units(&self.price, "order price")?),
			size: Shares::from_base_units(parse_units(&self.size, "order size")?),
			filled: Shares::from_base_units(parse_units(&self.filled, "order filled")?),
			notional_filled: Usdt::from_base_units(parse_units(&self.notional_filled, "order notional")?),
			fee_paid: Usdt::from_base_units(parse_units(&self.fee_paid, "order fee")?),
			reserved: match side {
				Side::Sell => Locked::Units(Shares::from_base_units(reserved)),
				Side::Buy => Locked::Cash(Usdt::from_base_units(reserved)),
			},
			state: OrderState::parse(&self.state)?,
			reject_reason: self.reject_reason,
			cancel_reason: self.cancel_reason.as_deref().map(CancelReason::parse).transpose()?,
		});
		Ok(OrderRecord {
			order,
			created_at: self.created_at,
			updated_at: self.updated_at,
		})
	}
}

#[derive(sqlx::FromRow)]
struct RestingRow {
	id: Uuid,
	user_id: Uuid,
	price: String,
	size: String,
	filled: String,
}

impl RestingRow {
	fn into_domain(self) -> Result<RestingOrder, DomainError> {
		let size = parse_units(&self.size, "order size")?;
		let filled = parse_units(&self.filled, "order filled")?;
		Ok(RestingOrder {
			id: OrderId::from_raw(self.id),
			user: UserId::from_raw(self.user_id),
			price: Price::from_base_units(parse_units(&self.price, "order price")?),
			remaining: Shares::from_base_units(size.saturating_sub(filled)),
		})
	}
}

#[derive(sqlx::FromRow)]
struct TradeRow {
	id: Uuid,
	service: String,
	buyer_id: Uuid,
	seller_id: Uuid,
	buy_order_id: Uuid,
	sell_order_id: Uuid,
	taker_side: String,
	price: String,
	size: String,
	notional: String,
	fee: String,
	executed_at: i64,
}

impl TradeRow {
	fn into_record(self) -> Result<TradeRecord, DomainError> {
		Ok(TradeRecord {
			trade: Trade {
				id: TradeId::from_raw(self.id),
				service: ServiceId::parse(&self.service)?,
				buyer: UserId::from_raw(self.buyer_id),
				seller: UserId::from_raw(self.seller_id),
				buy_order: OrderId::from_raw(self.buy_order_id),
				sell_order: OrderId::from_raw(self.sell_order_id),
				taker_side: Side::parse(&self.taker_side)?,
				price: Price::from_base_units(parse_units(&self.price, "trade price")?),
				size: Shares::from_base_units(parse_units(&self.size, "trade size")?),
				notional: Usdt::from_base_units(parse_units(&self.notional, "trade notional")?),
				fee: Usdt::from_base_units(parse_units(&self.fee, "trade fee")?),
			},
			executed_at: self.executed_at,
		})
	}
}

#[derive(sqlx::FromRow)]
struct LevelRow {
	price: String,
	size: String,
	orders: i32,
}

impl LevelRow {
	fn into_domain(self) -> Result<BookLevel, DomainError> {
		Ok(BookLevel {
			price: Price::from_base_units(parse_units(&self.price, "level price")?),
			size: Shares::from_base_units(parse_units(&self.size, "level size")?),
			orders: u32::try_from(self.orders).unwrap_or_default(),
		})
	}
}

#[derive(sqlx::FromRow)]
struct CandleRow {
	bucket: i64,
	open: String,
	high: String,
	low: String,
	close: String,
	volume: String,
}

impl CandleRow {
	fn into_domain(self) -> Result<Candle, DomainError> {
		Ok(Candle {
			time: self.bucket,
			open: Price::from_base_units(parse_units(&self.open, "candle open")?),
			high: Price::from_base_units(parse_units(&self.high, "candle high")?),
			low: Price::from_base_units(parse_units(&self.low, "candle low")?),
			close: Price::from_base_units(parse_units(&self.close, "candle close")?),
			volume: Shares::from_base_units(parse_units(&self.volume, "candle volume")?),
		})
	}
}

fn parse_units(raw: &str, what: &str) -> Result<u128, DomainError> {
	raw.parse::<u128>().map_err(|_| DomainError::Repository(format!("malformed {what}")))
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

fn order_not_found(id: OrderId) -> DomainError {
	DomainError::NotFound {
		entity: "order",
		id: id.to_string(),
	}
}

/// The book's single write lock. `hashtext` folds the slug into the advisory key space;
/// a collision with another key only serializes two unrelated writers, never corrupts.
async fn lock_book(conn: &mut PgConnection, service: &ServiceId) -> Result<(), DomainError> {
	sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
		.bind(service.as_str())
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(())
}

/// Bump the book's revision inside the writing transaction and return the new value.
async fn bump_revision(conn: &mut PgConnection, service: &ServiceId) -> Result<i64, DomainError> {
	sqlx::query_scalar::<_, i64>(
		"INSERT INTO book_revisions (service, revision) VALUES ($1, 1) \
		 ON CONFLICT (service) DO UPDATE SET revision = book_revisions.revision + 1, updated_at = now() RETURNING revision",
	)
	.bind(service.as_str())
	.fetch_one(&mut *conn)
	.await
	.map_err(repo_err)
}

async fn current_revision(conn: &mut PgConnection, service: &ServiceId) -> Result<u64, DomainError> {
	let revision = sqlx::query_scalar::<_, i64>("SELECT revision FROM book_revisions WHERE service = $1")
		.bind(service.as_str())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.unwrap_or_default();
	Ok(u64::try_from(revision).unwrap_or_default())
}

fn reserved_units(order: &Order) -> u128 {
	match order.reserved() {
		Locked::Units(units) => units.base_units(),
		Locked::Cash(cash) => cash.base_units(),
	}
}

async fn insert_order(conn: &mut PgConnection, order: &Order, revision: i64) -> Result<(), DomainError> {
	sqlx::query(
		"INSERT INTO book_orders (id, service, user_id, client_order_id, side, kind, tif, price, size, filled, notional_filled, fee_paid, reserved, state, revision) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
	)
	.bind(order.id().raw())
	.bind(order.service().as_str())
	.bind(order.user().raw())
	.bind(order.client_order_id().as_str())
	.bind(order.side().as_str())
	.bind(order.kind().as_str())
	.bind(order.tif().as_str())
	.bind(order.price().base_units().to_string())
	.bind(order.size().base_units().to_string())
	.bind(order.filled().base_units().to_string())
	.bind(order.notional_filled().base_units().to_string())
	.bind(order.fee_paid().base_units().to_string())
	.bind(reserved_units(order).to_string())
	.bind(order.state().as_str())
	.bind(revision)
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(())
}

/// Persist the mutable fields of an order the caller holds under the book lock.
async fn update_order(conn: &mut PgConnection, order: &Order, revision: i64) -> Result<(), DomainError> {
	let result = sqlx::query("UPDATE book_orders SET filled = $2, notional_filled = $3, fee_paid = $4, state = $5, cancel_reason = $6, revision = $7, updated_at = now() WHERE id = $1")
		.bind(order.id().raw())
		.bind(order.filled().base_units().to_string())
		.bind(order.notional_filled().base_units().to_string())
		.bind(order.fee_paid().base_units().to_string())
		.bind(order.state().as_str())
		.bind(order.cancel_reason().map(CancelReason::as_str))
		.bind(revision)
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	if result.rows_affected() != 1 {
		return Err(DomainError::Repository("order row vanished under the book lock".into()));
	}
	Ok(())
}

async fn load_order_for_update(conn: &mut PgConnection, id: OrderId) -> Result<OrderRecord, DomainError> {
	sqlx::query_as::<_, OrderRow>(SELECT_ORDER_BY_ID_FOR_UPDATE)
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(|| order_not_found(id))?
		.into_record()
}

async fn load_order(conn: &mut PgConnection, id: OrderId) -> Result<OrderRecord, DomainError> {
	sqlx::query_as::<_, OrderRow>(SELECT_ORDER_BY_ID)
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.ok_or_else(|| order_not_found(id))?
		.into_record()
}

async fn find_by_client_id(conn: &mut PgConnection, user: UserId, client_order_id: &ClientOrderId) -> Result<Option<OrderRecord>, DomainError> {
	sqlx::query_as::<_, OrderRow>(SELECT_ORDER_BY_CLIENT_ID)
		.bind(user.raw())
		.bind(client_order_id.as_str())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?
		.map(OrderRow::into_record)
		.transpose()
}

async fn record_event(conn: &mut PgConnection, aggregate: &str, aggregate_id: Uuid, event: &BookEvent) -> Result<(), DomainError> {
	let payload = serde_json::to_string(event).map_err(|e| DomainError::Repository(e.to_string()))?;
	outbox::insert_event(conn, Uuid::new_v4(), aggregate, aggregate_id, "book", &payload, true).await
}

/// The release of an order that just reached a terminal state, if its escrow still holds
/// anything. Nothing for an order that rests on, and nothing for one drained to zero.
async fn record_release(conn: &mut PgConnection, order: &Order) -> Result<(), DomainError> {
	let Some(released) = order.release() else {
		return Ok(());
	};
	record_event(
		conn,
		ORDER_AGGREGATE,
		order.id().raw(),
		&BookEvent::OrderReleased {
			order_id: order.id(),
			service: order.service().clone(),
			user: order.user(),
			side: order.side(),
			released,
		},
	)
	.await
}

async fn insert_trade(conn: &mut PgConnection, trade: &Trade) -> Result<TradeRecord, DomainError> {
	sqlx::query(
		"INSERT INTO book_trades (id, service, buyer_id, seller_id, buy_order_id, sell_order_id, taker_side, price, size, notional, fee) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
	)
	.bind(trade.id.raw())
	.bind(trade.service.as_str())
	.bind(trade.buyer.raw())
	.bind(trade.seller.raw())
	.bind(trade.buy_order.raw())
	.bind(trade.sell_order.raw())
	.bind(trade.taker_side.as_str())
	.bind(trade.price.base_units().to_string())
	.bind(trade.size.base_units().to_string())
	.bind(trade.notional.base_units().to_string())
	.bind(trade.fee.base_units().to_string())
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	sqlx::query_as::<_, TradeRow>(SELECT_TRADE_BY_ID)
		.bind(trade.id.raw())
		.fetch_one(&mut *conn)
		.await
		.map_err(repo_err)?
		.into_record()
}

fn policy_from_row(row: &sqlx::postgres::PgRow) -> Result<BookPolicyRecord, DomainError> {
	let service: String = row.try_get("service").map_err(repo_err)?;
	let price_tick: String = row.try_get("price_tick").map_err(repo_err)?;
	let lot_size: String = row.try_get("lot_size").map_err(repo_err)?;
	let taker_fee_bps: i32 = row.try_get("taker_fee_bps").map_err(repo_err)?;
	let market_slippage_bps: i32 = row.try_get("market_slippage_bps").map_err(repo_err)?;
	let policy = BookPolicy::new(
		row.try_get("book_open").map_err(repo_err)?,
		u32::try_from(taker_fee_bps).map_err(|_| DomainError::Repository("malformed taker fee".into()))?,
		Price::from_base_units(parse_units(&price_tick, "price tick")?),
		Shares::from_base_units(parse_units(&lot_size, "lot size")?),
		u32::try_from(market_slippage_bps).map_err(|_| DomainError::Repository("malformed market slippage".into()))?,
		row.try_get("allow_unbacked_trading").map_err(repo_err)?,
	)?;
	Ok(BookPolicyRecord {
		service: ServiceId::parse(&service)?,
		policy,
		updated_at: row.try_get("updated_at").map_err(repo_err)?,
	})
}

#[async_trait]
impl BookStore for PgBook {
	async fn policy(&self, service: &ServiceId) -> Result<Option<BookPolicyRecord>, DomainError> {
		let row = sqlx::query(
			"SELECT service, book_open, taker_fee_bps, price_tick, lot_size, market_slippage_bps, allow_unbacked_trading, \
			 EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at \
			 FROM book_policies WHERE service = $1",
		)
		.bind(service.as_str())
		.fetch_optional(&self.pool)
		.await
		.map_err(repo_err)?;
		row.as_ref().map(policy_from_row).transpose()
	}

	async fn set_policy(&self, service: &ServiceId, policy: &BookPolicy) -> Result<BookPolicyRecord, DomainError> {
		let row = sqlx::query(
			"INSERT INTO book_policies (service, book_open, taker_fee_bps, price_tick, lot_size, market_slippage_bps, allow_unbacked_trading) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7) \
			 ON CONFLICT (service) DO UPDATE SET book_open = EXCLUDED.book_open, taker_fee_bps = EXCLUDED.taker_fee_bps, price_tick = EXCLUDED.price_tick, \
			 lot_size = EXCLUDED.lot_size, market_slippage_bps = EXCLUDED.market_slippage_bps, allow_unbacked_trading = EXCLUDED.allow_unbacked_trading, \
			 updated_at = now() \
			 RETURNING service, book_open, taker_fee_bps, price_tick, lot_size, market_slippage_bps, allow_unbacked_trading, \
			 EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at",
		)
		.bind(service.as_str())
		.bind(policy.book_open())
		.bind(i32::try_from(policy.taker_fee_bps()).map_err(|_| DomainError::Validation("taker fee out of range".into()))?)
		.bind(policy.price_tick().base_units().to_string())
		.bind(policy.lot_size().base_units().to_string())
		.bind(i32::try_from(policy.market_slippage_bps()).map_err(|_| DomainError::Validation("market slippage out of range".into()))?)
		.bind(policy.allow_unbacked_trading())
		.fetch_one(&self.pool)
		.await
		.map_err(|err| match err.as_database_error().and_then(|db| db.constraint()) {
			// The row references the registry: terms for a product nobody registered are
			// a `NotFound`, not an internal error.
			Some("book_policies_service_fkey") => DomainError::NotFound {
				entity: "allocation",
				id: service.to_string(),
			},
			_ => repo_err(err),
		})?;
		policy_from_row(&row)
	}

	async fn find_by_client_id(&self, user: UserId, client_order_id: &ClientOrderId) -> Result<Option<OrderRecord>, DomainError> {
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		find_by_client_id(&mut conn, user, client_order_id).await
	}

	async fn find_order(&self, id: OrderId) -> Result<Option<OrderRecord>, DomainError> {
		sqlx::query_as::<_, OrderRow>(SELECT_ORDER_BY_ID)
			.bind(id.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?
			.map(OrderRow::into_record)
			.transpose()
	}

	async fn best_price(&self, service: &ServiceId, side: Side) -> Result<Option<Price>, DomainError> {
		let sql = match side {
			Side::Buy => SELECT_BEST_BID,
			Side::Sell => SELECT_BEST_ASK,
		};
		let raw = sqlx::query_scalar::<_, String>(sql).bind(service.as_str()).fetch_optional(&self.pool).await.map_err(repo_err)?;
		raw.map(|price| parse_units(&price, "best price").map(Price::from_base_units)).transpose()
	}

	async fn place(&self, mut order: Order, engine: &dyn MatchingEngine, policy: &BookPolicy, nav: Nav, guard: &dyn SpendGuard) -> Result<PlaceOutcome, DomainError> {
		let service = order.service().clone();
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		lock_book(&mut tx, &service).await?;
		match order.side() {
			// A buy spends the user's unified claim, so it takes the same shared per-user lock
			// a withdraw and a subscribe take (see [`outbox::lock_user`]) — the three
			// optimistic Read-Firsts on one claim serialize on one target.
			Side::Buy => outbox::lock_user(&mut tx, order.user().raw()).await?,
			// A sell spends units, which redemptions serialize on the position row.
			Side::Sell => {
				sqlx::query("SELECT 1 FROM fund_positions WHERE user_id = $1 AND service = $2 FOR UPDATE")
					.bind(order.user().raw())
					.bind(service.as_str())
					.fetch_optional(&mut *tx)
					.await
					.map_err(repo_err)?;
			}
		}
		// The idempotency read, repeated under the lock: two concurrent retries of one
		// request both passed the use case's unlocked read, and the second to arrive
		// here must find the first's row rather than trip the unique constraint.
		if let Some(existing) = find_by_client_id(&mut tx, order.user(), order.client_order_id()).await? {
			return Ok(PlaceOutcome::Existing(existing));
		}
		guard.check().await?;

		let (sql, side) = match order.side() {
			Side::Buy => (SELECT_ASKS_CROSSING, Side::Sell),
			Side::Sell => (SELECT_BIDS_CROSSING, Side::Buy),
		};
		let resting = sqlx::query_as::<_, RestingRow>(sql)
			.bind(service.as_str())
			.bind(order.price().base_units().to_string())
			.fetch_all(&mut *tx)
			.await
			.map_err(repo_err)?
			.into_iter()
			.map(RestingRow::into_domain)
			.collect::<Result<Vec<_>, _>>()?;
		let incoming = IncomingOrder {
			id: order.id(),
			user: order.user(),
			side: order.side(),
			tif: order.tif(),
			price: order.price(),
			size: order.size(),
		};
		let outcome = match engine.match_order(&incoming, &resting, policy) {
			Ok(outcome) => outcome,
			// Nothing was written: the transaction is dropped, the lock released.
			Err(rejection) => return Ok(PlaceOutcome::Rejected(rejection)),
		};

		let revision = bump_revision(&mut tx, &service).await?;
		// The taker's row is inserted BEFORE its fills are applied and the row updated at
		// the end, because the trades reference it; its escrow lock is the first fact
		// drained, because the relay must hold the escrow before any fill draws on it.
		insert_order(&mut tx, &order, revision).await?;
		record_event(
			&mut tx,
			ORDER_AGGREGATE,
			order.id().raw(),
			&BookEvent::OrderPlaced {
				order_id: order.id(),
				service: service.clone(),
				user: order.user(),
				side: order.side(),
				locked: order.reserved(),
			},
		)
		.await?;

		let mut trades = Vec::with_capacity(outcome.fills.len());
		for fill in &outcome.fills {
			let mut maker = load_order_for_update(&mut tx, fill.maker).await?.order;
			let notional = fill.price.value(fill.size)?;
			let fee = policy.taker_fee(notional)?;
			maker.fill(fill.size, fill.price, Usdt::ZERO)?;
			order.fill(fill.size, fill.price, fee)?;
			let (buyer, seller, buy_order, sell_order) = match side {
				// The maker is on `side`, the opposite of the incoming order.
				Side::Sell => (order.user(), maker.user(), order.id(), maker.id()),
				Side::Buy => (maker.user(), order.user(), maker.id(), order.id()),
			};
			let trade = Trade {
				id: TradeId::new(),
				service: service.clone(),
				buyer,
				seller,
				buy_order,
				sell_order,
				taker_side: order.side(),
				price: fill.price,
				size: fill.size,
				notional,
				fee,
			};
			trades.push(insert_trade(&mut tx, &trade).await?);
			record_event(
				&mut tx,
				TRADE_AGGREGATE,
				trade.id.raw(),
				&BookEvent::TradeExecuted {
					trade_id: trade.id,
					service: service.clone(),
					buyer,
					seller,
					buy_order_id: buy_order,
					sell_order_id: sell_order,
					taker_side: order.side(),
					size: fill.size,
					price: fill.price,
					notional,
					fee,
					nav,
					payee: Party::fee_payee(),
				},
			)
			.await?;
			update_order(&mut tx, &maker, revision).await?;
			record_release(&mut tx, &maker).await?;
		}
		if !outcome.rests && order.state().is_resting() {
			// An IOC (or market) remainder: recorded as cancelled, its escrow handed back.
			order.cancel(CancelReason::remainder_of(order.kind()))?;
		}
		update_order(&mut tx, &order, revision).await?;
		record_release(&mut tx, &order).await?;
		let record = load_order(&mut tx, order.id()).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(PlaceOutcome::Placed {
			order: record,
			trades,
			revision: u64::try_from(revision).unwrap_or_default(),
		})
	}

	async fn cancel(&self, id: OrderId) -> Result<CancelOutcome, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The service is needed to take the book lock, and the row is re-read under it: a
		// placement admitted between the two reads may have filled the order.
		let service = load_order(&mut tx, id).await?.order.service().clone();
		lock_book(&mut tx, &service).await?;
		let mut record = load_order_for_update(&mut tx, id).await?;
		if record.order.state() == OrderState::Cancelled {
			let revision = current_revision(&mut tx, &service).await?;
			return Ok(CancelOutcome { order: record, revision });
		}
		record.order.cancel(CancelReason::User)?;
		let revision = bump_revision(&mut tx, &service).await?;
		update_order(&mut tx, &record.order, revision).await?;
		record_release(&mut tx, &record.order).await?;
		let order = load_order(&mut tx, id).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(CancelOutcome {
			order,
			revision: u64::try_from(revision).unwrap_or_default(),
		})
	}

	async fn list_open(&self, user: UserId, service: Option<&ServiceId>) -> Result<Vec<OrderRecord>, DomainError> {
		let rows = sqlx::query_as::<_, OrderRow>(SELECT_OPEN_ORDERS)
			.bind(user.raw())
			.bind(service.map(ServiceId::as_str))
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(OrderRow::into_record).collect()
	}

	async fn list_history(&self, user: UserId, service: Option<&ServiceId>, limit: u32) -> Result<Vec<OrderRecord>, DomainError> {
		let rows = sqlx::query_as::<_, OrderRow>(SELECT_ORDER_HISTORY)
			.bind(user.raw())
			.bind(service.map(ServiceId::as_str))
			.bind(i64::from(limit))
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(OrderRow::into_record).collect()
	}

	async fn list_user_trades(&self, user: UserId, service: Option<&ServiceId>, limit: u32) -> Result<Vec<UserTrade>, DomainError> {
		let rows = sqlx::query_as::<_, TradeRow>(SELECT_USER_TRADES)
			.bind(user.raw())
			.bind(service.map(ServiceId::as_str))
			.bind(i64::from(limit))
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter()
			.map(|row| {
				let record = row.into_record()?;
				let trade = &record.trade;
				let side = if trade.buyer == user { Side::Buy } else { Side::Sell };
				let order_id = match side {
					Side::Buy => trade.buy_order,
					Side::Sell => trade.sell_order,
				};
				// The fee is the taker's alone.
				let fee = if trade.taker_side == side { trade.fee } else { Usdt::ZERO };
				Ok(UserTrade { trade: record, side, order_id, fee })
			})
			.collect()
	}

	async fn depth(&self, service: &ServiceId, depth: u32) -> Result<BookDepth, DomainError> {
		let limit = i64::from(depth);
		let bids = sqlx::query_as::<_, LevelRow>(SELECT_BID_LEVELS)
			.bind(service.as_str())
			.bind(limit)
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?
			.into_iter()
			.map(LevelRow::into_domain)
			.collect::<Result<Vec<_>, _>>()?;
		let asks = sqlx::query_as::<_, LevelRow>(SELECT_ASK_LEVELS)
			.bind(service.as_str())
			.bind(limit)
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?
			.into_iter()
			.map(LevelRow::into_domain)
			.collect::<Result<Vec<_>, _>>()?;
		let mut conn = self.pool.acquire().await.map_err(repo_err)?;
		let revision = current_revision(&mut conn, service).await?;
		drop(conn);
		let last = sqlx::query_as::<_, (String, String)>("SELECT price, taker_side FROM book_trades WHERE service = $1 ORDER BY seq DESC LIMIT 1")
			.bind(service.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?
			.map(|(price, side)| Ok::<_, DomainError>((Price::from_base_units(parse_units(&price, "last price")?), Side::parse(&side)?)))
			.transpose()?;
		// The 24h reference: the last trade at or before the window opened — the price the
		// window "started" at — or, for a book younger than a day, its very first trade.
		let reference_24h = sqlx::query_scalar::<_, String>("SELECT price FROM book_trades WHERE service = $1 AND executed_at <= now() - interval '24 hours' ORDER BY seq DESC LIMIT 1")
			.bind(service.as_str())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		let reference_24h = match reference_24h {
			Some(price) => Some(price),
			None => sqlx::query_scalar::<_, String>("SELECT price FROM book_trades WHERE service = $1 ORDER BY seq ASC LIMIT 1")
				.bind(service.as_str())
				.fetch_optional(&self.pool)
				.await
				.map_err(repo_err)?,
		};
		let reference_24h = reference_24h.map(|price| parse_units(&price, "reference price").map(Price::from_base_units)).transpose()?;
		let volume_24h = sqlx::query_scalar::<_, String>("SELECT COALESCE(SUM(size::numeric), 0)::text FROM book_trades WHERE service = $1 AND executed_at > now() - interval '24 hours'")
			.bind(service.as_str())
			.fetch_one(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(BookDepth {
			revision,
			bids,
			asks,
			last,
			reference_24h,
			volume_24h: Shares::from_base_units(parse_units(&volume_24h, "24h volume")?),
		})
	}

	async fn list_trades(&self, service: &ServiceId, limit: u32) -> Result<Vec<TradeRecord>, DomainError> {
		let rows = sqlx::query_as::<_, TradeRow>(SELECT_TAPE)
			.bind(service.as_str())
			.bind(i64::from(limit))
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(TradeRow::into_record).collect()
	}

	async fn candles(&self, service: &ServiceId, resolution: CandleResolution, from: i64, to: i64) -> Result<Vec<Candle>, DomainError> {
		let rows = sqlx::query_as::<_, CandleRow>(SELECT_CANDLES)
			.bind(service.as_str())
			.bind(from)
			.bind(to)
			.bind(resolution.seconds())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(CandleRow::into_domain).collect()
	}

	async fn orders_revision(&self, user: UserId, service: &ServiceId) -> Result<u64, DomainError> {
		let revision = sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(revision) FROM book_orders WHERE user_id = $1 AND service = $2")
			.bind(user.raw())
			.bind(service.as_str())
			.fetch_one(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(revision.and_then(|value| u64::try_from(value).ok()).unwrap_or_default())
	}
}

/// Mark an order the ledger refused to escrow as `rejected` — the relay's write, after
/// the `OrderPlaced` fact parked. The order stands on the book with nothing behind it
/// until this lands, so it is taken off (no longer resting, never matched again) and the
/// caller sees why. Its own revision is bumped so a watching client refetches.
pub async fn mark_rejected(pool: &PgPool, order_id: Uuid, reason: &str) -> Result<(), sqlx::Error> {
	let mut tx = pool.begin().await?;
	let service: Option<String> = sqlx::query_scalar("SELECT service FROM book_orders WHERE id = $1 AND state IN ('open', 'partially_filled')")
		.bind(order_id)
		.fetch_optional(&mut *tx)
		.await?;
	let Some(service) = service else {
		return Ok(());
	};
	let revision: i64 = sqlx::query_scalar(
		"INSERT INTO book_revisions (service, revision) VALUES ($1, 1) \
		 ON CONFLICT (service) DO UPDATE SET revision = book_revisions.revision + 1, updated_at = now() RETURNING revision",
	)
	.bind(&service)
	.fetch_one(&mut *tx)
	.await?;
	sqlx::query("UPDATE book_orders SET state = 'rejected', reject_reason = $2, revision = $3, updated_at = now() WHERE id = $1 AND state IN ('open', 'partially_filled')")
		.bind(order_id)
		.bind(reason)
		.bind(revision)
		.execute(&mut *tx)
		.await?;
	tx.commit().await
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_order_and_trade_select_reads_the_documented_columns() {
		for sql in [
			SELECT_ORDER_BY_ID,
			SELECT_ORDER_BY_ID_FOR_UPDATE,
			SELECT_ORDER_BY_CLIENT_ID,
			SELECT_OPEN_ORDERS,
			SELECT_ORDER_HISTORY,
		] {
			assert!(sql.contains(ORDER_COLUMNS), "an order SELECT drifted from ORDER_COLUMNS: {sql}");
		}
		for sql in [SELECT_USER_TRADES, SELECT_TAPE, SELECT_TRADE_BY_ID] {
			assert!(sql.contains(TRADE_COLUMNS), "a trade SELECT drifted from TRADE_COLUMNS: {sql}");
		}
	}
}
