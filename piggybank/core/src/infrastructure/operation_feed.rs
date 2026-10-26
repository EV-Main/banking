//! Postgres adapter for the [`OperationFeed`] port — the caller's activity timeline.
//!
//! One `UNION ALL` over the projections the write side already maintains (`deposits`,
//! `withdrawals`), ordered and paged in the database. Doing the merge here rather than in
//! separate round-trips is the whole point of the port: `LIMIT` applies to the *merged*
//! stream, so a user with a long deposit history still sees their most recent withdrawal.
//!
//! A plain pool read — a projection, never a fact write, so no transaction. The rows
//! are read across aggregates but each is a self-consistent snapshot of its own row;
//! the timeline makes no cross-row invariant claim, so it needs no snapshot isolation.
//!
//! `party_id` on `deposits` is TEXT while `withdrawals` keys on a UUID `user_id` — hence
//! the two bindings of the same caller.

use async_trait::async_trait;
use domain::{
	error::DomainError,
	money::{Network, TxRef, Usdt},
	users::UserId,
	withdrawals::WithdrawalState,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::ports::operations::{Operation, OperationFeed, OperationPage};

/// The merged feed. The inner branches select `created_at` as a `timestamptz` so the
/// ordering keeps sub-second resolution — truncating to epoch seconds first would make
/// same-second operations tie. `id` breaks a genuine tie deterministically, so paging
/// never drops or repeats a row.
const FEED_SQL: &str = "\
SELECT kind, id, state, EXTRACT(EPOCH FROM ts)::bigint AS created_at, amount, fee, network, address, tx_ref FROM (
    SELECT 'deposit' AS kind, tx_ref AS id, 'credited' AS state, created_at AS ts,
           amount, NULL::text AS fee, network, NULL::text AS address, tx_ref
      FROM deposits WHERE party_kind = 'user' AND party_id = $1
    UNION ALL
    SELECT 'withdrawal', id::text, state, created_at,
           amount, fee, network, address, tx_ref
      FROM withdrawals WHERE user_id = $2
) AS feed
ORDER BY ts DESC, id
LIMIT $3";

/// The raw shape of one merged row, before it is narrowed back to an [`Operation`]
/// variant. Every value column is nullable because the union is a widening of disjoint
/// shapes — the narrowing below is what re-establishes which are guaranteed.
type FeedRow = (
	String,         // kind
	String,         // id
	String,         // state
	i64,            // created_at (unix seconds)
	Option<String>, // amount (base units)
	Option<String>, // fee
	Option<String>, // network
	Option<String>, // address
	Option<String>, // tx_ref
);

pub struct PgOperationFeed {
	pool: PgPool,
}

impl PgOperationFeed {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// A column the row's `kind` guarantees is present. A `NULL` here means the projection
/// and this adapter disagree about the shape of a row, which is a repository fault, not
/// a domain one — surfaced rather than defaulted, because silently reading a missing
/// amount as zero would put a wrong number in front of a user.
fn required<T>(value: Option<T>, column: &str, kind: &str) -> Result<T, DomainError> {
	value.ok_or_else(|| DomainError::Repository(format!("operation feed: {kind} row missing {column}")))
}

/// Base-unit integer strings are how every money column is stored (see the migrations'
/// `~ '^[0-9]+$'` checks). Parsing is fallible only if that invariant is broken.
fn base_units(raw: &str, column: &str) -> Result<u128, DomainError> {
	raw.parse::<u128>().map_err(|_| DomainError::Repository(format!("operation feed: malformed {column}")))
}

fn amount(raw: &str, column: &str) -> Result<Usdt, DomainError> {
	Ok(Usdt::from_base_units(base_units(raw, column)?))
}

fn id(raw: &str) -> Result<Uuid, DomainError> {
	Uuid::parse_str(raw).map_err(|_| DomainError::Repository("operation feed: malformed id".into()))
}

fn narrow(row: FeedRow) -> Result<Operation, DomainError> {
	let (kind, row_id, state, created_at, amount_raw, fee_raw, network_raw, address_raw, tx_ref_raw) = row;
	match kind.as_str() {
		"deposit" => Ok(Operation::Deposit {
			tx_ref: TxRef::parse(&row_id)?,
			network: Network::parse(&required(network_raw, "network", &kind)?)?,
			amount: amount(&required(amount_raw, "amount", &kind)?, "deposit amount")?,
			created_at,
		}),
		"withdrawal" => {
			let network = Network::parse(&required(network_raw, "network", &kind)?)?;
			Ok(Operation::Withdrawal {
				id: id(&row_id)?,
				network,
				// Passed through as stored — see the field's note on `Operation::Withdrawal`.
				address: required(address_raw, "address", &kind)?,
				amount: amount(&required(amount_raw, "amount", &kind)?, "withdrawal amount")?,
				fee: amount(&required(fee_raw, "fee", &kind)?, "withdrawal fee")?,
				state: WithdrawalState::parse(&state)?,
				// NULL until the withdrawal settles — genuinely absent, not a shape fault.
				tx_ref: tx_ref_raw.as_deref().map(TxRef::parse).transpose()?,
				created_at,
			})
		}
		other => Err(DomainError::Repository(format!("operation feed: unknown kind {other}"))),
	}
}

#[async_trait]
impl OperationFeed for PgOperationFeed {
	async fn list_by_user(&self, user: UserId, limit: u32) -> Result<OperationPage, DomainError> {
		// Over-fetch by one: if the extra row comes back the history is longer than the
		// page, which is the only thing `truncated` claims. Cheaper and race-free
		// compared with a second COUNT over the same union.
		let probe = i64::from(limit) + 1;
		let rows = sqlx::query_as::<_, FeedRow>(FEED_SQL)
			.bind(user.to_string())
			.bind(user.raw())
			.bind(probe)
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		let truncated = rows.len() as i64 > i64::from(limit);
		let operations = rows.into_iter().take(limit as usize).map(narrow).collect::<Result<Vec<_>, _>>()?;
		Ok(OperationPage { operations, truncated })
	}
}
