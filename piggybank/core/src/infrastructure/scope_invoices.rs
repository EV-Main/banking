//! Postgres adapter for [`InvoiceStore`] — invoices scopes issue and users pay
//! (`scope_invoices`, migration 0056) under the scope ledger's idempotency keys for the issuer
//! and the payer's own key for the payment — plus the reaper's expiry of open invoices past
//! `expires_at`. A payment's ledger leg is the relay's, from the outbox event written in the
//! same transaction as the paid row; every other step writes its money event itself.

use async_trait::async_trait;
use domain::{
	architecture::Id,
	error::DomainError,
	money::Usdt,
	scope_invoices::{Invoice, InvoiceEvent, InvoicePayment, InvoiceRefusal, InvoiceState, InvoiceTerms},
	scope_ledger::{ClientId, LedgerRefusal, SubjectRef},
	scopes::{Compartment, ScopeAccountLabel, ScopeId},
	users::UserId,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
	infrastructure::{
		claims::TxReads,
		compartments, money_events, outbox, outflow,
		scope_ledger::{PgScopeLedger, find_request, insert_request, key_lock, owner_on, raced_request},
	},
	ports::{
		scope_invoices::{InvoiceIssue, InvoiceStore, IssueFacts, Paid, PaymentAdmission, PaymentError, PaymentFacts, RecordedInvoice},
		scope_ledger::{Recorded, RequestKey, TransferError},
	},
};

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

macro_rules! select_invoice {
	($tail:literal) => {
		concat!(
			"SELECT id, client_id, scope, to_label, amount, memo, payer_auth_subject, subject_ref, state, version, ",
			"EXTRACT(EPOCH FROM expires_at)::BIGINT AS expires_at, paid_by_user, paid_by_auth_subject, paid_from, ",
			"EXTRACT(EPOCH FROM paid_at)::BIGINT AS paid_at, EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at, ",
			"EXTRACT(EPOCH FROM now())::BIGINT AS read_at FROM scope_invoices ",
			$tail
		)
	};
}

fn recorded_invoice(row: &PgRow) -> Result<RecordedInvoice, DomainError> {
	let id: Uuid = row.try_get("id").map_err(repo_err)?;
	let corrupt = |e: DomainError| DomainError::Repository(format!("scope invoice {id}: {e}"));
	let text = |column: &str| row.try_get::<String, _>(column).map_err(repo_err);
	let raw_amount = text("amount")?;
	let amount = raw_amount
		.parse::<u128>()
		.map(Usdt::from_base_units)
		.map_err(|_| corrupt(DomainError::Validation(format!("amount is not a base-unit amount: {raw_amount:?}"))))?;
	let terms = InvoiceTerms {
		client: ClientId::parse(&text("client_id")?).map_err(corrupt)?,
		scope: ScopeId::parse(&text("scope")?).map_err(corrupt)?,
		to: ScopeAccountLabel::parse(&text("to_label")?).map_err(corrupt)?,
		amount,
		memo: text("memo")?,
		expires_at: row.try_get("expires_at").map_err(repo_err)?,
		payer_auth_subject: row.try_get("payer_auth_subject").map_err(repo_err)?,
		subject_ref: SubjectRef::parse(&text("subject_ref")?).map_err(corrupt)?,
	};
	let paid_by: Option<Uuid> = row.try_get("paid_by_user").map_err(repo_err)?;
	let payment = match paid_by {
		None => None,
		Some(payer) => {
			let missing = |column: &str| corrupt(DomainError::Validation(format!("paid, but {column} is not set")));
			Some(InvoicePayment {
				payer: UserId::from_raw(payer),
				payer_auth_subject: row
					.try_get::<Option<String>, _>("paid_by_auth_subject")
					.map_err(repo_err)?
					.ok_or_else(|| missing("paid_by_auth_subject"))?,
				from: Compartment::parse(&row.try_get::<Option<String>, _>("paid_from").map_err(repo_err)?.ok_or_else(|| missing("paid_from"))?).map_err(corrupt)?,
				paid_at: row.try_get::<Option<i64>, _>("paid_at").map_err(repo_err)?.ok_or_else(|| missing("paid_at"))?,
			})
		}
	};
	let version: i64 = row.try_get("version").map_err(repo_err)?;
	let version = u64::try_from(version).map_err(|_| corrupt(DomainError::Validation(format!("negative version {version}"))))?;
	let state = InvoiceState::parse(&text("state")?).map_err(corrupt)?;
	Ok(RecordedInvoice {
		invoice: Invoice::restore(Id::from_raw(id), terms, state, version, payment).map_err(corrupt)?,
		created_at: row.try_get("created_at").map_err(repo_err)?,
		read_at: row.try_get("read_at").map_err(repo_err)?,
	})
}

/// The database's clock: one clock for every expiry, whichever replica asks.
async fn db_now(conn: &mut PgConnection) -> Result<i64, DomainError> {
	sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT").fetch_one(conn).await.map_err(repo_err)
}

async fn locked(conn: &mut PgConnection, id: Uuid) -> Result<Option<RecordedInvoice>, DomainError> {
	sqlx::query(select_invoice!("WHERE id = $1 FOR UPDATE"))
		.bind(id)
		.fetch_optional(conn)
		.await
		.map_err(repo_err)?
		.as_ref()
		.map(recorded_invoice)
		.transpose()
}

fn version(invoice: &Invoice) -> Result<i64, DomainError> {
	i64::try_from(invoice.version()).map_err(|_| DomainError::Repository(format!("scope invoice {}: version overflows", invoice.id())))
}

/// Record a cancellation or an expiry: the row, the event log, and the issuer's money event,
/// on the caller's transaction.
async fn record_closed(conn: &mut PgConnection, invoice: &mut Invoice) -> Result<(), DomainError> {
	sqlx::query("UPDATE scope_invoices SET state = $2, version = $3, updated_at = now() WHERE id = $1")
		.bind(invoice.id().raw())
		.bind(invoice.state().as_str())
		.bind(version(invoice)?)
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	money_events::append(&mut *conn, &money_events::invoice_step(invoice)?).await?;
	outbox::drain_to_outbox_by(conn, invoice, InvoiceEvent::moves_money).await
}

#[async_trait]
impl InvoiceStore for PgScopeLedger {
	async fn invoice(&self, id: Uuid) -> Result<Option<RecordedInvoice>, DomainError> {
		sqlx::query(select_invoice!("WHERE id = $1"))
			.bind(id)
			.fetch_optional(self.pool())
			.await
			.map_err(repo_err)?
			.as_ref()
			.map(recorded_invoice)
			.transpose()
	}

	async fn issued_invoice(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<Option<RecordedInvoice>, DomainError> {
		sqlx::query(select_invoice!("WHERE id = $1 AND scope = $2 AND client_id = $3"))
			.bind(id)
			.bind(scope.as_str())
			.bind(client.as_str())
			.fetch_optional(self.pool())
			.await
			.map_err(repo_err)?
			.as_ref()
			.map(recorded_invoice)
			.transpose()
	}

	async fn auth_subject(&self, user: UserId) -> Result<Option<String>, DomainError> {
		sqlx::query_scalar("SELECT auth_subject FROM users WHERE id = $1")
			.bind(user.raw())
			.fetch_optional(self.pool())
			.await
			.map_err(repo_err)
	}

	async fn record_invoice(&self, key: RequestKey<'_>, terms: &str, to: &ScopeAccountLabel, admission: &dyn InvoiceIssue) -> Result<Recorded<RecordedInvoice>, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		outbox::lock_named(&mut tx, &key_lock(&key)).await?;
		if let Some(existing) = find_request(&mut tx, &key).await? {
			tx.commit().await.map_err(repo_err)?;
			return Ok(Recorded::Existing(existing));
		}
		let facts = IssueFacts {
			scope: compartments::scope_for_share(&mut tx, key.scope).await?,
			destination_owner: owner_on(&mut tx, key.scope, to).await?,
			now: db_now(&mut tx).await?,
		};
		// The refusal rolls the transaction back with nothing written.
		let mut invoice = admission.admit(&facts).await?;
		let issued = invoice.terms().clone();
		sqlx::query(
			"INSERT INTO scope_invoices (id, client_id, scope, to_label, amount, memo, payer_auth_subject, subject_ref, expires_at) \
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, to_timestamp($9))",
		)
		.bind(invoice.id().raw())
		.bind(issued.client.as_str())
		.bind(issued.scope.as_str())
		.bind(issued.to.as_str())
		.bind(issued.amount.base_units().to_string())
		.bind(&issued.memo)
		.bind(&issued.payer_auth_subject)
		.bind(issued.subject_ref.as_str())
		.bind(issued.expires_at as f64)
		.execute(&mut *tx)
		.await
		.map_err(repo_err)?;
		if !insert_request(&mut tx, &key, terms, &invoice.id().to_string()).await? {
			drop(tx);
			return Ok(Recorded::Existing(raced_request(self.pool(), &key).await?));
		}
		money_events::append(&mut tx, &money_events::invoice_step(&invoice)?).await?;
		outbox::drain_to_outbox_by(&mut tx, &mut invoice, InvoiceEvent::moves_money).await?;
		tx.commit().await.map_err(repo_err)?;
		Ok(Recorded::New(RecordedInvoice {
			invoice,
			created_at: facts.now,
			read_at: facts.now,
		}))
	}

	async fn cancel_invoice(&self, client: &ClientId, scope: &ScopeId, id: Uuid) -> Result<RecordedInvoice, TransferError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		// Another client's invoice is the same NOT_FOUND as none: a client cannot learn which
		// invoice ids exist.
		let not_found = || DomainError::NotFound {
			entity: "invoice",
			id: id.to_string(),
		};
		let mut recorded = locked(&mut tx, id).await?.ok_or_else(not_found)?;
		let terms = recorded.invoice.terms();
		if &terms.client != client || &terms.scope != scope {
			return Err(not_found().into());
		}
		let now = db_now(&mut tx).await?;
		match recorded.invoice.cancel(now) {
			Ok(true) => record_closed(&mut tx, &mut recorded.invoice).await?,
			Ok(false) => {}
			Err(InvoiceRefusal::NotOpen) => return Err(TransferError::Refused(LedgerRefusal::InvoiceNotOpen)),
			Err(other) => return Err(TransferError::Internal(format!("invoice {id}: a cancel refused as {}", other.reason()))),
		}
		tx.commit().await.map_err(repo_err)?;
		recorded.read_at = now;
		Ok(recorded)
	}

	async fn pay_invoice(&self, payer: UserId, id: Uuid, from: &Compartment, idempotency_key: &str, admission: &dyn PaymentAdmission) -> Result<Paid, PaymentError> {
		let mut tx = self.pool().begin().await.map_err(repo_err)?;
		// The key first: two clicks under one key may name different sources, so the claim
		// locks alone would let both through to the unique index.
		outbox::lock_named(&mut tx, &format!("invoice_pay_key:{payer}:{idempotency_key}")).await?;
		let earlier = sqlx::query(select_invoice!("WHERE paid_by_user = $1 AND pay_idempotency_key = $2"))
			.bind(payer.raw())
			.bind(idempotency_key)
			.fetch_optional(&mut *tx)
			.await
			.map_err(repo_err)?;
		if let Some(row) = earlier {
			let earlier = recorded_invoice(&row)?;
			tx.commit().await.map_err(repo_err)?;
			return Ok(Paid::Earlier(earlier));
		}
		// The row before the claim: a second payer of the same invoice waits here and then finds
		// it paid. Nothing that holds a claim lock ever waits on an invoice row, so the order
		// cannot deadlock.
		let mut recorded = locked(&mut tx, id).await?.ok_or_else(|| DomainError::NotFound {
			entity: "invoice",
			id: id.to_string(),
		})?;
		// The lock every spender of this claim takes (a move, a hold, a withdrawal, another
		// payment), so the spendable the admission reads cannot be spent twice.
		outbox::lock_claim(&mut tx, &from.claim_key(payer)).await?;
		let facts = PaymentFacts {
			scope: compartments::scope_for_share(&mut tx, &recorded.invoice.terms().scope).await?,
			payer: outflow::standing(&mut *tx, payer).await?,
			now: db_now(&mut tx).await?,
		};
		// The refusal rolls the transaction back with nothing written.
		admission.admit(&mut recorded.invoice, &facts, &mut TxReads::new(&mut tx)).await?;
		let payment = recorded
			.invoice
			.payment()
			.cloned()
			.ok_or_else(|| DomainError::Repository(format!("invoice {id}: admitted without a payment")))?;
		let updated = sqlx::query(
			"UPDATE scope_invoices SET state = 'paid', version = $2, paid_by_user = $3, paid_by_auth_subject = $4, paid_from = $5, \
			 paid_at = to_timestamp($6), pay_idempotency_key = $7, updated_at = now() WHERE id = $1",
		)
		.bind(id)
		.bind(version(&recorded.invoice)?)
		.bind(payment.payer.raw())
		.bind(&payment.payer_auth_subject)
		.bind(payment.from.as_str())
		.bind(payment.paid_at as f64)
		.bind(idempotency_key)
		.execute(&mut *tx)
		.await;
		match updated {
			Ok(_) => {}
			// The key lock above makes this unreachable; should it ever not, the payer is told
			// the key is taken, never an internal error.
			Err(sqlx::Error::Database(err)) if err.is_unique_violation() => return Err(DomainError::Conflict("idempotency_key was used for a different payment".into()).into()),
			Err(err) => return Err(repo_err(err).into()),
		}
		outbox::drain_to_outbox_by(&mut tx, &mut recorded.invoice, InvoiceEvent::moves_money).await?;
		tx.commit().await.map_err(repo_err)?;
		recorded.read_at = facts.now;
		Ok(Paid::Now(recorded))
	}
}

/// Expire one invoice past its expiry; `false` when it was paid, cancelled or expired since
/// the scan.
async fn expire_one(pool: &PgPool, id: Uuid) -> Result<bool, DomainError> {
	let mut tx = pool.begin().await.map_err(repo_err)?;
	let Some(mut recorded) = locked(&mut tx, id).await? else { return Ok(false) };
	let now = db_now(&mut tx).await?;
	if !recorded.invoice.expire(now) {
		return Ok(false);
	}
	record_closed(&mut tx, &mut recorded.invoice).await?;
	tx.commit().await.map_err(repo_err)?;
	Ok(true)
}

/// Expire every open invoice past its expiry, a batch at a time — the reaper's step, which is
/// what puts `KIND_INVOICE_EXPIRED` in the issuer's stream whether or not anyone reads the
/// invoice again. Until it runs, every read already shows such an invoice expired and a payment
/// is refused under the row lock, so the sweep's cadence never decides whether a late payment
/// goes through. An invoice that cannot be read is logged and left for the next sweep. Returns
/// how many it expired.
pub async fn expire_due_invoices(pool: &PgPool) -> Result<usize, sqlx::Error> {
	let due: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM scope_invoices WHERE state = 'open' AND expires_at <= now() ORDER BY expires_at LIMIT 100")
		.fetch_all(pool)
		.await?;
	let mut expired = 0;
	for id in due {
		match expire_one(pool, id).await {
			Ok(true) => expired += 1,
			Ok(false) => {}
			Err(err) => tracing::warn!(invoice_id = %id, "reaper: could not expire a scope invoice: {err}"),
		}
	}
	Ok(expired)
}
