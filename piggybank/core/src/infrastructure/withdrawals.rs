//! Postgres adapter for the [`WithdrawalRepository`] port.
//!
//! Mirrors [`PgRedemptions`](super::redemptions::PgRedemptions): the command methods
//! are atomic and **row-locked** — load `FOR UPDATE`, apply the aggregate command
//! inside the lock (the aggregate is the single authority on a transition's validity),
//! then persist the new state together with the drained events (event_log + outbox)
//! in one transaction. A transition emits an event only when it actually happens, so
//! an idempotent re-settle/re-fail writes no outbox row and the relay moves money
//! exactly once.

use async_trait::async_trait;
use domain::{
	architecture::{Reader, Repository},
	error::DomainError,
	money::{Network, TxRef, Usdt, WalletAddress},
	payments::PaymentId,
	users::UserId,
	withdrawals::{Withdrawal, WithdrawalId, WithdrawalSource, WithdrawalState},
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
	infrastructure::{claims::TxReads, outbox, scope_notifications},
	ports::{
		WithdrawalRepository,
		audit::RequestAudit,
		ledger::SpendGuard,
		security_notices,
		withdrawals::{AddressHold, Opened, QueuedWithdrawal, Screening, Usage},
	},
};

const SELECT_BY_ID: &str = "SELECT id, source, user_id, payment_id, network, address, amount, fee, state, tx_ref FROM withdrawals WHERE id = $1";
const SELECT_BY_ID_FOR_UPDATE: &str = "SELECT id, source, user_id, payment_id, network, address, amount, fee, state, tx_ref FROM withdrawals WHERE id = $1 FOR UPDATE";
const SELECT_BY_KEY: &str = "SELECT id, source, user_id, payment_id, network, address, amount, fee, state, tx_ref FROM withdrawals WHERE user_id = $1 AND idempotency_key = $2";
const SELECT_BY_USER: &str =
	"SELECT id, source, user_id, payment_id, network, address, amount, fee, state, tx_ref FROM withdrawals WHERE user_id = $1 AND source = 'user' ORDER BY created_at DESC";

/// Why a queued withdrawal to a first-seen destination does not dispatch yet.
pub const HELD_FOR_NEW_ADDRESS: &str = "the withdrawal is held until the new-address delay has passed";

/// Why a held withdrawal is not released even though its time has come: the user's sign-in
/// details changed after it was requested, so a person must look at it first. It stays
/// `queued` (the user can cancel it; the reaper takes it at 24 h).
pub const HELD_FOR_REVIEW: &str = "the withdrawal is held for review: the account's sign-in details changed after it was requested";

/// SQL boolean over `withdrawals w`: a held-address withdrawal (`release_at` set) requested
/// before the owner's last credential change. A macro so the dispatcher's sweep and the
/// repository splice the same text into a `&'static str` query.
#[macro_export]
macro_rules! withdrawal_needs_review {
	() => {
		"(w.release_at IS NOT NULL AND EXISTS (SELECT 1 FROM users u WHERE u.id = w.user_id AND u.credential_changed_at > w.created_at))"
	};
}

/// The address-book key of a destination. EVM addresses differ only in checksum case, so the
/// book compares them lowercased — otherwise changing a letter's case would make a held
/// address look released, or a released one look new. TRON base58 and TON friendly forms are
/// case-sensitive and kept as written (TON's raw/friendly spellings of one account are not
/// unified: a follow-up).
pub fn book_key(network: Network, address: &WalletAddress) -> String {
	match network {
		Network::Bep20 | Network::Polygon => address.as_str().to_ascii_lowercase(),
		Network::Trc20 | Network::Ton => address.as_str().to_owned(),
	}
}

pub struct PgWithdrawals {
	pool: PgPool,
}

impl PgWithdrawals {
	pub fn new(pool: PgPool) -> Self {
		Self { pool }
	}
}

impl Repository for PgWithdrawals {
	type Aggregate = Withdrawal;
}

impl Reader for PgWithdrawals {
	type Aggregate = Withdrawal;
}

#[derive(sqlx::FromRow)]
struct WithdrawalRow {
	id: Uuid,
	/// `user` | `treasury` (0052). Paired with `user_id` and `payment_id` by schema CHECKs.
	source: String,
	user_id: Option<Uuid>,
	payment_id: Option<Uuid>,
	network: String,
	address: String,
	amount: String,
	fee: String,
	state: String,
	tx_ref: Option<String>,
}

impl WithdrawalRow {
	fn source(&self) -> Result<WithdrawalSource, DomainError> {
		source_of(self.id, &self.source, self.user_id, self.payment_id)
	}

	fn into_domain(self) -> Result<Withdrawal, DomainError> {
		let source = self.source()?;
		let network = Network::parse(&self.network)?;
		let amount = Usdt::from_base_units(self.amount.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal amount".into()))?);
		let fee = Usdt::from_base_units(self.fee.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal fee".into()))?);
		let address = WalletAddress::parse(network, &self.address)?;
		let tx_ref = self.tx_ref.as_deref().map(TxRef::parse).transpose()?;
		Ok(Withdrawal::rehydrate(
			WithdrawalId::from_raw(self.id),
			source,
			network,
			address,
			amount,
			fee,
			WithdrawalState::parse(&self.state)?,
			tx_ref,
		))
	}
}

/// The `(source, user_id, payment_id)` columns back into a source; the schema pairs them
/// by CHECK.
fn source_of(id: Uuid, source: &str, user_id: Option<Uuid>, payment_id: Option<Uuid>) -> Result<WithdrawalSource, DomainError> {
	match (source, user_id, payment_id) {
		("user", Some(user), None) => Ok(WithdrawalSource::User(UserId::from_raw(user))),
		("treasury", None, Some(payment)) => Ok(WithdrawalSource::Treasury {
			payment: PaymentId::from_raw(payment),
		}),
		_ => Err(DomainError::Repository(format!("withdrawal {id} has an inconsistent source"))),
	}
}

/// Put the destination into the user's book if it is not there, under the claim lock.
/// Returns `(new, held)`: whether this request added it, and whether its release time is still
/// ahead (always for a new one when `hold_secs > 0`). Database time throughout — the release
/// is compared with the dispatcher's clock, which is the same one.
async fn enter_address_book(conn: &mut PgConnection, user: UserId, withdrawal: &Withdrawal, hold_secs: u64) -> Result<(bool, bool), DomainError> {
	let inserted: Option<bool> = sqlx::query_scalar(
		"INSERT INTO withdrawal_addresses (user_id, network, address, release_at) VALUES ($1, $2, $3, now() + make_interval(secs => $4)) \
		 ON CONFLICT (user_id, network, address) DO NOTHING RETURNING release_at > now()",
	)
	.bind(user.raw())
	.bind(withdrawal.network().as_str())
	.bind(book_key(withdrawal.network(), withdrawal.address()))
	.bind(hold_secs as f64)
	.fetch_optional(&mut *conn)
	.await
	.map_err(repo_err)?;
	if let Some(held) = inserted {
		return Ok((true, held));
	}
	let held: bool = sqlx::query_scalar("SELECT release_at > now() FROM withdrawal_addresses WHERE user_id = $1 AND network = $2 AND address = $3")
		.bind(user.raw())
		.bind(withdrawal.network().as_str())
		.bind(book_key(withdrawal.network(), withdrawal.address()))
		.fetch_one(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok((false, held))
}

/// A cancelled withdrawal to a destination still inside its hold, with nothing ever completed
/// there, takes the destination back out of the book: otherwise request-then-cancel would
/// start the clock for free, and the real request would find the address released and ship at
/// once. A released destination (its time has passed) or one that has paid out stays.
async fn forget_unreleased_address(conn: &mut PgConnection, withdrawal: &Withdrawal) -> Result<(), DomainError> {
	let Some(user) = withdrawal.user() else {
		return Ok(());
	};
	sqlx::query(
		"DELETE FROM withdrawal_addresses a WHERE a.user_id = $1 AND a.network = $2 AND a.address = $3 AND a.release_at > now() \
		 AND NOT EXISTS (SELECT 1 FROM withdrawals w WHERE w.user_id = a.user_id AND w.network = a.network AND w.state = 'completed' \
		   AND CASE WHEN w.network IN ('bep20', 'polygon') THEN lower(w.address) ELSE w.address END = a.address)",
	)
	.bind(user.raw())
	.bind(withdrawal.network().as_str())
	.bind(book_key(withdrawal.network(), withdrawal.address()))
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(())
}

/// Whether the user's credentials changed within the last `secs` seconds, by database time.
async fn in_cooloff(conn: &mut PgConnection, user: UserId, secs: u64) -> Result<bool, DomainError> {
	sqlx::query_scalar("SELECT COALESCE(credential_changed_at > now() - make_interval(secs => $2), FALSE) FROM users WHERE id = $1")
		.bind(user.raw())
		.bind(secs as f64)
		.fetch_one(&mut *conn)
		.await
		.map_err(repo_err)
}

/// The user's gross over the last 24 hours and the count over the last hour, failed and
/// cancelled withdrawals excluded: their money came back, so they no longer weigh on the limit.
async fn recent_usage(conn: &mut PgConnection, user: UserId) -> Result<Usage, DomainError> {
	let (gross, count): (String, i64) = sqlx::query_as(
		"SELECT COALESCE(SUM(amount::numeric), 0)::text, COUNT(*) FILTER (WHERE created_at > now() - interval '1 hour') \
		 FROM withdrawals WHERE user_id = $1 AND source = 'user' AND created_at > now() - interval '24 hours' AND state NOT IN ('failed', 'cancelled')",
	)
	.bind(user.raw())
	.fetch_one(&mut *conn)
	.await
	.map_err(repo_err)?;
	let gross = gross.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal sum".into()))?;
	Ok(Usage {
		gross_24h: Usdt::from_base_units(gross),
		count_1h: u32::try_from(count).unwrap_or(u32::MAX),
	})
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

async fn insert_row(conn: &mut PgConnection, withdrawal: &Withdrawal, idempotency_key: Option<&str>, audit: Option<&RequestAudit>) -> Result<(), DomainError> {
	// Empty means "the caller said nothing", which the table keeps as NULL.
	let audited = |value: fn(&RequestAudit) -> &String| audit.map(value).filter(|v| !v.is_empty());
	sqlx::query(
		"INSERT INTO withdrawals (id, source, user_id, payment_id, network, address, amount, fee, state, tx_ref, idempotency_key, origin, client_ip, user_agent, intent_id, stepup_method) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
	)
	.bind(withdrawal.id().raw())
	.bind(withdrawal.source().column())
	.bind(withdrawal.user().map(|user| user.raw()))
	.bind(withdrawal.source().payment().map(|payment| payment.raw()))
	.bind(withdrawal.network().as_str())
	.bind(withdrawal.address().as_str())
	.bind(withdrawal.amount().base_units().to_string())
	.bind(withdrawal.fee().base_units().to_string())
	.bind(withdrawal.state().as_str())
	.bind(withdrawal.tx_ref().map(TxRef::as_str))
	.bind(idempotency_key)
	.bind(audited(|a| &a.origin))
	.bind(audited(|a| &a.client_ip))
	.bind(audited(|a| &a.user_agent))
	.bind(audited(|a| &a.intent_id))
	.bind(audited(|a| &a.stepup_method))
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(())
}

/// Persist a state transition (settle/fail) — `state` and `tx_ref` are the only
/// mutable columns. We hold the row lock, so exactly one row must update.
/// Cancel a still-queued withdrawal on the caller's open transaction: load `FOR UPDATE`,
/// apply [`Withdrawal::cancel`] (which refuses anything past `Queued` — a broadcast may have
/// landed), persist, drain the void to the outbox. Crate-visible so the payments adapter can
/// void the withdrawal an L1 execution created in the SAME transaction that refuses that
/// execution over a moved consent pin — the one place two aggregates must move together,
/// because leaving the withdrawal queued would let a revoked consent's money ship anyway.
pub(crate) async fn cancel_on(conn: &mut PgConnection, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
	let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
		.bind(id.raw())
		.fetch_optional(&mut *conn)
		.await
		.map_err(repo_err)?;
	let mut withdrawal = row
		.ok_or_else(|| DomainError::NotFound {
			entity: "withdrawal",
			id: id.to_string(),
		})?
		.into_domain()?;
	withdrawal.cancel()?;
	update_row(conn, &withdrawal).await?;
	outbox::drain_to_outbox(conn, &mut withdrawal, true).await?;
	forget_unreleased_address(conn, &withdrawal).await?;
	Ok(withdrawal)
}

async fn update_row(conn: &mut PgConnection, withdrawal: &Withdrawal) -> Result<(), DomainError> {
	let result = sqlx::query("UPDATE withdrawals SET state = $2, tx_ref = $3, updated_at = now() WHERE id = $1")
		.bind(withdrawal.id().raw())
		.bind(withdrawal.state().as_str())
		.bind(withdrawal.tx_ref().map(TxRef::as_str))
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	if result.rows_affected() != 1 {
		return Err(DomainError::Repository("withdrawal row vanished under lock".into()));
	}
	Ok(())
}

#[async_trait]
impl WithdrawalRepository for PgWithdrawals {
	async fn open(&self, withdrawal: &mut Withdrawal, guard: &dyn SpendGuard) -> Result<(), DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// Serialize every flow that spends this claim on one shared lock target (see
		// [`outbox::lock_user`]) so two of them can't both pass the optimistic Read-First
		// and both park a reserve (TB's flag is the money backstop; this lock keeps the PG
		// projection from diverging). For a user that's withdraw + subscribe on their
		// unified claim.
		outbox::lock_claim(&mut tx, &withdrawal.source().claim_key()).await?;
		guard.check(&mut TxReads::new(&mut tx)).await?;
		insert_row(&mut tx, withdrawal, None, None).await?;
		outbox::drain_to_outbox(&mut tx, withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(())
	}

	async fn open_keyed(&self, withdrawal: &mut Withdrawal, guard: &dyn SpendGuard, user: UserId, key: &str) -> Result<Option<Withdrawal>, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		// The same lock `open` takes, so two requests of one user with one key are serialized
		// and the second finds the first's row here instead of failing on the unique index.
		outbox::lock_claim(&mut tx, &withdrawal.source().claim_key()).await?;
		let existing = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_KEY)
			.bind(user.raw())
			.bind(key)
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		if let Some(row) = existing {
			return row.into_domain().map(Some);
		}
		guard.check(&mut TxReads::new(&mut tx)).await?;
		insert_row(&mut tx, withdrawal, Some(key), None).await?;
		outbox::drain_to_outbox(&mut tx, withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(None)
	}

	async fn open_screened(&self, withdrawal: &mut Withdrawal, guard: &dyn SpendGuard, key: Option<&str>, screening: &Screening<'_>) -> Result<Opened, DomainError> {
		let WithdrawalSource::User(user) = withdrawal.source() else {
			return Err(DomainError::Validation("only a user's withdrawal is screened".into()));
		};
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		outbox::lock_claim(&mut tx, &withdrawal.source().claim_key()).await?;
		if let Some(key) = key {
			let existing = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_KEY)
				.bind(user.raw())
				.bind(key)
				.fetch_optional(&mut *tx)
				.await
				.map_err(repo_err)?;
			if let Some(row) = existing {
				return row.into_domain().map(Opened::Replayed);
			}
		}
		guard.check(&mut TxReads::new(&mut tx)).await?;
		// Under the claim lock, so two racing requests cannot both read the same headroom.
		let usage = recent_usage(&mut tx, user).await?;
		screening.profile.check(withdrawal.amount(), usage)?;
		let (new_address, held) = enter_address_book(&mut tx, user, withdrawal, screening.hold_secs).await?;
		if new_address && screening.cooloff_secs > 0 && in_cooloff(&mut tx, user, screening.cooloff_secs).await? {
			// Dropping the transaction takes the address-book row back out with it.
			return Err(DomainError::Precondition(
				"withdrawals to a new address are paused for a while after your sign-in details changed; use an address you have withdrawn to before, or try again later".into(),
			));
		}
		if held && withdrawal.state() != WithdrawalState::Queued {
			// The caller read the book before the lock and a twin request put the destination in
			// since: a dispatched withdrawal cannot be taken back, so nothing is recorded.
			return Err(DomainError::Precondition("this destination was just added to your address book; try again".into()));
		}
		insert_row(&mut tx, withdrawal, key, Some(screening.audit)).await?;
		if held {
			sqlx::query(
				"UPDATE withdrawals w SET release_at = a.release_at FROM withdrawal_addresses a \
				 WHERE w.id = $1 AND a.user_id = $2 AND a.network = $3 AND a.address = $4",
			)
			.bind(withdrawal.id().raw())
			.bind(user.raw())
			.bind(withdrawal.network().as_str())
			.bind(book_key(withdrawal.network(), withdrawal.address()))
			.execute(&mut *tx)
			.await
			.map_err(repo_err)?;
		}
		outbox::drain_to_outbox(&mut tx, withdrawal, true).await?;
		// In the transaction of the fact it announces: a withdrawal that exists is a withdrawal
		// the owner is told about, and one that rolled back is not.
		let notice = security_notices::withdrawal_requested(user, withdrawal, held.then_some(screening.hold_secs));
		scope_notifications::enqueue(&mut tx, &notice, None).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(Opened::Recorded { new_address })
	}

	async fn address_hold(&self, user: UserId, network: Network, address: &WalletAddress) -> Result<AddressHold, DomainError> {
		let row: Option<bool> = sqlx::query_scalar("SELECT release_at > now() FROM withdrawal_addresses WHERE user_id = $1 AND network = $2 AND address = $3")
			.bind(user.raw())
			.bind(network.as_str())
			.bind(book_key(network, address))
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		Ok(match row {
			None => AddressHold::New,
			Some(true) => AddressHold::Held,
			Some(false) => AddressHold::Released,
		})
	}

	async fn find_by_key(&self, user: UserId, key: &str) -> Result<Option<Withdrawal>, DomainError> {
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_KEY)
			.bind(user.raw())
			.bind(key)
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(WithdrawalRow::into_domain).transpose()
	}

	async fn dispatch(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
			.bind(id.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		let mut withdrawal = row
			.ok_or_else(|| DomainError::NotFound {
				entity: "withdrawal",
				id: id.to_string(),
			})?
			.into_domain()?;
		// The row lock is held, so the hold cannot move between this read and the transition.
		// Every payout path funnels through here, which is why the operator RPC cannot skip it.
		let (held, review): (bool, bool) = sqlx::query_as(concat!(
			"SELECT COALESCE(w.release_at > now(), FALSE), ",
			crate::withdrawal_needs_review!(),
			" FROM withdrawals w WHERE w.id = $1"
		))
		.bind(id.raw())
		.fetch_one(&mut *tx)
		.await
		.map_err(repo_err)?;
		if held {
			return Err(DomainError::Precondition(HELD_FOR_NEW_ADDRESS.into()));
		}
		if review {
			return Err(DomainError::Precondition(HELD_FOR_REVIEW.into()));
		}
		withdrawal.dispatch()?;
		update_row(&mut tx, &withdrawal).await?;
		outbox::drain_to_outbox(&mut tx, &mut withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn settle(&self, id: WithdrawalId, tx_ref: TxRef) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
			.bind(id.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		let mut withdrawal = row
			.ok_or_else(|| DomainError::NotFound {
				entity: "withdrawal",
				id: id.to_string(),
			})?
			.into_domain()?;
		withdrawal.settle(tx_ref)?;
		update_row(&mut tx, &withdrawal).await?;
		outbox::drain_to_outbox(&mut tx, &mut withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn fail(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID_FOR_UPDATE)
			.bind(id.raw())
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		let mut withdrawal = row
			.ok_or_else(|| DomainError::NotFound {
				entity: "withdrawal",
				id: id.to_string(),
			})?
			.into_domain()?;
		withdrawal.fail()?;
		update_row(&mut tx, &withdrawal).await?;
		outbox::drain_to_outbox(&mut tx, &mut withdrawal, true).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn cancel(&self, id: WithdrawalId) -> Result<Withdrawal, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let withdrawal = cancel_on(&mut tx, id).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(withdrawal)
	}

	async fn find_by_id(&self, id: WithdrawalId) -> Result<Option<Withdrawal>, DomainError> {
		let row = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_ID)
			.bind(id.raw())
			.fetch_optional(&self.pool)
			.await
			.map_err(repo_err)?;
		row.map(WithdrawalRow::into_domain).transpose()
	}

	async fn list_by_user(&self, user: UserId) -> Result<Vec<Withdrawal>, DomainError> {
		let rows = sqlx::query_as::<_, WithdrawalRow>(SELECT_BY_USER)
			.bind(user.raw())
			.fetch_all(&self.pool)
			.await
			.map_err(repo_err)?;
		rows.into_iter().map(WithdrawalRow::into_domain).collect()
	}

	async fn list_actionable(&self) -> Result<Vec<QueuedWithdrawal>, DomainError> {
		let rows = sqlx::query_as::<_, (Uuid, String, Option<Uuid>, Option<Uuid>, Option<String>, String, String, String, String, String, i64)>(
			"SELECT w.id, w.source, w.user_id, w.payment_id, u.email, w.network, w.address, w.amount, w.fee, w.state, EXTRACT(EPOCH FROM w.created_at)::BIGINT \
			 FROM withdrawals w LEFT JOIN users u ON u.id = w.user_id \
			 WHERE w.state IN ('queued', 'processing') ORDER BY w.created_at ASC",
		)
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;
		rows.into_iter()
			.map(|(id, source, user_id, payment_id, email, network, address, amount, fee, state, created_at)| {
				let amount = Usdt::from_base_units(amount.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal amount".into()))?);
				let fee = Usdt::from_base_units(fee.parse::<u128>().map_err(|_| DomainError::Repository("malformed withdrawal fee".into()))?);
				let source = source_of(id, &source, user_id, payment_id)?;
				Ok(QueuedWithdrawal {
					id: WithdrawalId::from_raw(id),
					source,
					email: email.unwrap_or_default(),
					network: Network::parse(&network)?,
					address,
					amount,
					net_amount: amount.checked_sub(fee).unwrap_or(Usdt::ZERO),
					state,
					created_at,
				})
			})
			.collect()
	}
}
