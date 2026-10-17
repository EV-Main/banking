//! The money-event log scope services pull (`scope.v1.MoneyEvents`, migration 0055): who is
//! shown what is decided here, once, when the event is written, from who owned what at that
//! moment — `Pull` only reads a client's own deliveries by cursor.
//!
//! An account's opening is written with the account. A movement is written by the relay right
//! after the ledger took it: the relay is the single writer of the ledger, so the balances
//! read at that point are the balances right after the event and nothing else. Every event id
//! derives from what the event is about, so a relay that applies an event twice (at-least-once
//! delivery) records it once.

use async_trait::async_trait;
use domain::{
	architecture::DomainEvent,
	balance::LedgerAccountKey,
	error::DomainError,
	money::Usdt,
	scope_charges::ChargeEvent,
	scope_invoices::{Invoice, InvoiceEvent, InvoiceState},
	scope_ledger::{ClientId, HoldDestination, HoldEvent, MoneyEventKind, ScopeLedgerEvent},
	scope_operations::ScopePayoutEvent,
	scopes::{ScopeAccountLabel, ScopeId},
	users::UserId,
};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
	infrastructure::{outbox, outbox::OutboxRow, scope_ledger::PgScopeLedger},
	ports::{
		ledger::{Ledger, LedgerError},
		scope_ledger::{EventAccount, EventBalance, MoneyEventRecord, MoneyEventStore, OwnedAccount},
	},
};

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// A money event about to be written, and who is to receive it.
#[derive(Clone, Debug)]
pub struct NewMoneyEvent {
	pub event_id: Uuid,
	pub scope: ScopeId,
	pub kind: MoneyEventKind,
	pub ref_id: String,
	pub aggregate_version: u64,
	pub auth_subject: String,
	pub amount: Option<Usdt>,
	pub from: Option<EventAccount>,
	pub to: Option<EventAccount>,
	pub subject_ref: String,
	pub initiator_client: String,
	pub balances: Vec<EventBalance>,
	pub hold_remaining: Option<Usdt>,
	/// The other scope of a movement between two scopes; empty otherwise.
	pub counterparty_scope: String,
	pub failure_reason: String,
	/// An invoice payment's source: `main` or the scope; empty for every other kind.
	pub paid_from: String,
	/// Every client the event is delivered to; banking itself is never one.
	pub audience: Vec<ClientId>,
}

/// The stored shape of one `balances` entry.
#[derive(Deserialize, Serialize)]
struct StoredBalance {
	label: String,
	owner: String,
	balance: String,
}

pub(crate) fn event_id(name: &str) -> Uuid {
	Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("money_event:{name}").as_bytes())
}

fn split(account: Option<&EventAccount>) -> (Option<&str>, Option<&str>) {
	match account {
		Some(EventAccount::Account(label)) => (Some(label.as_str()), None),
		Some(EventAccount::User(subject)) => (None, Some(subject.as_str())),
		None => (None, None),
	}
}

/// Why the money event of an applied outbox row was not written.
#[derive(Debug)]
pub enum MoneyEventError {
	/// Postgres or the ledger could not be reached, or gave up under load or a lock: waited
	/// out like any outage.
	Unavailable(String),
	/// The rows cannot make an event — undecodable, missing, inconsistent — or the database
	/// refused it for a reason a retry does not change: tried a bounded number of times, then
	/// parked alone.
	Unrecordable(String),
}

impl std::fmt::Display for MoneyEventError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Unavailable(reason) => write!(f, "unavailable: {reason}"),
			Self::Unrecordable(reason) => f.write_str(reason),
		}
	}
}

impl MoneyEventError {
	fn corrupt(reason: String) -> Self {
		Self::Unrecordable(reason)
	}

	/// Classes 08 (connection), 40 (rollback: serialization, deadlock), 53 (resources), 57
	/// (operator intervention: cancel, shutdown) and `55P03` (lock not available) pass with
	/// time; so do a pool that timed out or closed and a broken socket.
	fn db(context: impl std::fmt::Display, err: sqlx::Error) -> Self {
		let transient = match &err {
			sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::WorkerCrashed => true,
			sqlx::Error::Database(db) => db
				.code()
				.is_some_and(|code| ["08", "40", "53", "57"].iter().any(|class| code.starts_with(class)) || code == "55P03"),
			_ => false,
		};
		let reason = format!("{context}: {err}");
		if transient { Self::Unavailable(reason) } else { Self::Unrecordable(reason) }
	}

	fn ledger(context: impl std::fmt::Display, err: LedgerError) -> Self {
		let reason = format!("{context}: {err}");
		match err {
			LedgerError::Unavailable(_) | LedgerError::Retryable(_) => Self::Unavailable(reason),
			LedgerError::InsufficientFunds | LedgerError::Conflict(_) => Self::Unrecordable(reason),
		}
	}
}

/// Write `event` and its deliveries. A repeat of an event already written is a no-op. Appends
/// serialize on one lock held to commit, so positions become visible in the order they were
/// taken and a cursor never passes a position that commits later.
pub async fn append(conn: &mut PgConnection, event: &NewMoneyEvent) -> Result<(), DomainError> {
	append_checked(conn, event).await.map_err(|err| DomainError::Repository(err.to_string()))
}

/// [`append`], telling an outage from an event that cannot be written.
async fn append_checked(conn: &mut PgConnection, event: &NewMoneyEvent) -> Result<(), MoneyEventError> {
	outbox::lock_named_on(conn, "money_events_append")
		.await
		.map_err(|e| MoneyEventError::db("money_events_append lock", e))?;
	let balances: Vec<StoredBalance> = event
		.balances
		.iter()
		.map(|entry| StoredBalance {
			label: entry.label.as_str().to_owned(),
			owner: entry.owner.as_str().to_owned(),
			balance: entry.balance.base_units().to_string(),
		})
		.collect();
	let balances = serde_json::to_string(&balances).map_err(|e| MoneyEventError::corrupt(format!("money event balances: {e}")))?;
	let version = i64::try_from(event.aggregate_version).map_err(|_| MoneyEventError::corrupt(format!("money event version {} overflows", event.aggregate_version)))?;
	let (from_label, from_subject) = split(event.from.as_ref());
	let (to_label, to_subject) = split(event.to.as_ref());
	let position: Option<i64> = sqlx::query_scalar(
		"INSERT INTO money_events (event_id, scope, kind, ref_id, aggregate_version, auth_subject, amount, from_label, from_subject, to_label, to_subject, \
		 subject_ref, initiator_client, balances, hold_remaining, counterparty_scope, failure_reason, paid_from) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14::jsonb, $15, $16, $17, $18) \
		 ON CONFLICT (event_id) DO NOTHING RETURNING position",
	)
	.bind(event.event_id)
	.bind(event.scope.as_str())
	.bind(event.kind.as_str())
	.bind(&event.ref_id)
	.bind(version)
	.bind(&event.auth_subject)
	.bind(event.amount.map(|amount| amount.base_units().to_string()).unwrap_or_default())
	.bind(from_label)
	.bind(from_subject)
	.bind(to_label)
	.bind(to_subject)
	.bind(&event.subject_ref)
	.bind(&event.initiator_client)
	.bind(balances)
	.bind(event.hold_remaining.map(|remaining| remaining.base_units().to_string()))
	.bind(&event.counterparty_scope)
	.bind(&event.failure_reason)
	.bind(&event.paid_from)
	.fetch_optional(&mut *conn)
	.await
	.map_err(|e| MoneyEventError::db(format!("money event {}", event.event_id), e))?;
	let Some(position) = position else { return Ok(()) };
	let mut audience: Vec<&str> = event.audience.iter().filter(|client| !client.is_banking()).map(ClientId::as_str).collect();
	audience.sort_unstable();
	audience.dedup();
	sqlx::query("INSERT INTO money_event_deliveries (client_id, position, scope) SELECT client, $2, $3 FROM unnest($1::text[]) AS client")
		.bind(&audience)
		.bind(position)
		.bind(event.scope.as_str())
		.execute(&mut *conn)
		.await
		.map_err(|e| MoneyEventError::db(format!("money event {} deliveries", event.event_id), e))?;
	Ok(())
}

/// The opening of `account`, for its owner.
pub fn account_opened(account: &OwnedAccount) -> NewMoneyEvent {
	NewMoneyEvent {
		event_id: event_id(&format!("account_opened:{}:{}", account.scope, account.label)),
		scope: account.scope.clone(),
		kind: MoneyEventKind::AccountOpened,
		ref_id: String::new(),
		aggregate_version: 1,
		auth_subject: String::new(),
		amount: None,
		from: None,
		to: Some(EventAccount::Account(account.label.clone())),
		subject_ref: String::new(),
		initiator_client: account.owner.as_str().to_owned(),
		balances: vec![EventBalance {
			label: account.label.clone(),
			owner: account.owner.clone(),
			balance: Usdt::ZERO,
		}],
		hold_remaining: None,
		counterparty_scope: String::new(),
		failure_reason: String::new(),
		paid_from: String::new(),
		audience: vec![account.owner.clone()],
	}
}

/// What the relay's money event needs about a scope account: who owns it, and its posted
/// balance now.
async fn balance_of(pool: &PgPool, ledger: &dyn Ledger, scope: &ScopeId, label: &ScopeAccountLabel) -> Result<Option<EventBalance>, MoneyEventError> {
	let owner: Option<String> = sqlx::query_scalar("SELECT owner_client FROM scope_accounts WHERE scope = $1 AND label = $2")
		.bind(scope.as_str())
		.bind(label.as_str())
		.fetch_optional(pool)
		.await
		.map_err(|e| MoneyEventError::db(format!("owner of {scope}:{label}"), e))?;
	let Some(owner) = owner else { return Ok(None) };
	let owner = ClientId::parse(&owner).map_err(|e| MoneyEventError::corrupt(format!("owner of {scope}:{label}: {e}")))?;
	let balance = ledger
		.balance(&LedgerAccountKey::ScopeAccount(scope.clone(), label.clone()))
		.await
		.map_err(|e| MoneyEventError::ledger(format!("balance of {scope}:{label}"), e))?;
	Ok(Some(EventBalance {
		label: label.clone(),
		owner,
		balance: Usdt::from_base_units(balance.posted),
	}))
}

async fn auth_subject_of(pool: &PgPool, user: UserId) -> Result<String, MoneyEventError> {
	sqlx::query_scalar("SELECT auth_subject FROM users WHERE id = $1")
		.bind(user.raw())
		.fetch_one(pool)
		.await
		.map_err(|e| MoneyEventError::db(format!("auth_subject of user {user}"), e))
}

/// Record the money event of an outbox row the relay just applied to the ledger; a kind with
/// no money event is a no-op. Called after the ledger took the row and before it is marked
/// dispatched, so a failure here is retried with the row.
pub async fn record_relayed(pool: &PgPool, ledger: &dyn Ledger, row: &OutboxRow) -> Result<(), MoneyEventError> {
	let events = if row.kind == ScopeLedgerEvent::KIND {
		let event: ScopeLedgerEvent = serde_json::from_str(&row.payload).map_err(|e| MoneyEventError::corrupt(format!("{} payload: {e}", row.kind)))?;
		transferred(pool, ledger, event).await?
	} else if row.kind == HoldEvent::KIND {
		let event: HoldEvent = serde_json::from_str(&row.payload).map_err(|e| MoneyEventError::corrupt(format!("{} payload: {e}", row.kind)))?;
		vec![hold_event(pool, ledger, event).await?]
	} else if row.kind == ScopePayoutEvent::KIND {
		let event: ScopePayoutEvent = serde_json::from_str(&row.payload).map_err(|e| MoneyEventError::corrupt(format!("{} payload: {e}", row.kind)))?;
		paid_out(pool, ledger, event).await?
	} else if row.kind == InvoiceEvent::KIND {
		let event: InvoiceEvent = serde_json::from_str(&row.payload).map_err(|e| MoneyEventError::corrupt(format!("{} payload: {e}", row.kind)))?;
		match invoice_paid(pool, ledger, event).await? {
			Some(event) => vec![event],
			None => return Ok(()),
		}
	} else if row.kind == ChargeEvent::KIND {
		let event: ChargeEvent = serde_json::from_str(&row.payload).map_err(|e| MoneyEventError::corrupt(format!("{} payload: {e}", row.kind)))?;
		vec![charged(pool, ledger, event).await?]
	} else {
		return Ok(());
	};
	let mut tx = pool.begin().await.map_err(|e| MoneyEventError::db("begin", e))?;
	for event in &events {
		append_checked(&mut tx, event).await?;
	}
	tx.commit().await.map_err(|e| MoneyEventError::db("commit", e))
}

/// A transfer inside one scope is one event naming both accounts. One between two scopes is
/// one event per side (`events.proto`), each in its own scope, naming the other as the
/// counterparty and showing only its own side's balance — so each side's owner learns of its
/// own account and nothing of the other scope's.
async fn transferred(pool: &PgPool, ledger: &dyn Ledger, event: ScopeLedgerEvent) -> Result<Vec<NewMoneyEvent>, MoneyEventError> {
	let ScopeLedgerEvent::Transferred {
		transfer_id,
		scope,
		from_label,
		to_scope,
		to_label,
		amount,
	} = event;
	// `swept_by` is set on banking's fee route only: the client whose FEE_SWEEP it routes.
	let (client, subject_ref, swept_by): (String, String, Option<String>) =
		sqlx::query_as("SELECT t.client_id, t.subject_ref, s.client_id FROM scope_transfers t LEFT JOIN scope_transfers s ON s.id = t.fee_route_of WHERE t.id = $1")
			.bind(transfer_id.raw())
			.fetch_one(pool)
			.await
			.map_err(|e| MoneyEventError::db(format!("scope transfer {transfer_id}"), e))?;
	let initiator = ClientId::parse(&client).map_err(|e| MoneyEventError::corrupt(format!("scope transfer {transfer_id}: {e}")))?;
	let swept_by = swept_by
		.map(|raw| ClientId::parse(&raw))
		.transpose()
		.map_err(|e| MoneyEventError::corrupt(format!("scope transfer {transfer_id}: {e}")))?;
	let from_balance = balance_of(pool, ledger, &scope, &from_label).await?;
	let to_balance = balance_of(pool, ledger, &to_scope, &to_label).await?;
	let side = |event_name: String, side_scope: ScopeId, counterparty: String, balances: Vec<EventBalance>, shown_to_sweeper: bool| {
		let mut audience = vec![initiator.clone()];
		audience.extend(balances.iter().map(|entry| entry.owner.clone()));
		// banking owns both ends of a route out of a non-invest scope and `main` has no client,
		// so nobody would learn the money left `fee`: the client that swept it in does, on the
		// scope's side. It sees only what it is party to — the sweep it made — and no balance.
		if shown_to_sweeper {
			audience.extend(swept_by.clone());
		}
		NewMoneyEvent {
			event_id: event_id(&event_name),
			scope: side_scope,
			kind: MoneyEventKind::Transferred,
			ref_id: transfer_id.to_string(),
			aggregate_version: 1,
			auth_subject: String::new(),
			amount: Some(amount),
			from: Some(EventAccount::Account(from_label.clone())),
			to: Some(EventAccount::Account(to_label.clone())),
			subject_ref: subject_ref.clone(),
			// Empty when banking itself moved the money (`events.proto`).
			initiator_client: if initiator.is_banking() { String::new() } else { initiator.as_str().to_owned() },
			balances,
			hold_remaining: None,
			counterparty_scope: counterparty,
			failure_reason: String::new(),
			paid_from: String::new(),
			audience,
		}
	};
	if scope == to_scope {
		let balances = from_balance.into_iter().chain(to_balance).collect();
		return Ok(vec![side(format!("transferred:{transfer_id}"), scope, String::new(), balances, true)]);
	}
	Ok(vec![
		side(
			format!("transferred:{transfer_id}"),
			scope.clone(),
			to_scope.as_str().to_owned(),
			from_balance.into_iter().collect(),
			true,
		),
		side(
			format!("transferred:{transfer_id}:in"),
			to_scope,
			scope.as_str().to_owned(),
			to_balance.into_iter().collect(),
			false,
		),
	])
}

/// One PAID_OUT per line (`events.proto`), sharing the payout id, each showing the source's
/// balance after the whole chain — the chain is applied at once, so there is no balance between
/// its lines.
async fn paid_out(pool: &PgPool, ledger: &dyn Ledger, event: ScopePayoutEvent) -> Result<Vec<NewMoneyEvent>, MoneyEventError> {
	let ScopePayoutEvent::PaidOut { payout_id, scope, from_label, .. } = event;
	let (client, subject_ref): (String, String) = sqlx::query_as("SELECT client_id, subject_ref FROM scope_payouts WHERE id = $1")
		.bind(payout_id.raw())
		.fetch_one(pool)
		.await
		.map_err(|e| MoneyEventError::db(format!("scope payout {payout_id}"), e))?;
	let initiator = ClientId::parse(&client).map_err(|e| MoneyEventError::corrupt(format!("scope payout {payout_id}: {e}")))?;
	let lines: Vec<(i32, String, String)> = sqlx::query_as("SELECT line_no, auth_subject, amount FROM scope_payout_lines WHERE payout_id = $1 ORDER BY line_no")
		.bind(payout_id.raw())
		.fetch_all(pool)
		.await
		.map_err(|e| MoneyEventError::db(format!("scope payout {payout_id} lines"), e))?;
	let balances: Vec<EventBalance> = balance_of(pool, ledger, &scope, &from_label).await?.into_iter().collect();
	let mut audience = vec![initiator.clone()];
	audience.extend(balances.iter().map(|entry| entry.owner.clone()));
	lines
		.into_iter()
		.map(|(line_no, auth_subject, raw)| {
			let amount = raw
				.parse::<u128>()
				.map(Usdt::from_base_units)
				.map_err(|_| MoneyEventError::corrupt(format!("scope payout {payout_id}: line {line_no} amount {raw:?}")))?;
			Ok(NewMoneyEvent {
				event_id: event_id(&format!("paid_out:{payout_id}:{line_no}")),
				scope: scope.clone(),
				kind: MoneyEventKind::PaidOut,
				ref_id: payout_id.to_string(),
				aggregate_version: 1,
				auth_subject: auth_subject.clone(),
				amount: Some(amount),
				from: Some(EventAccount::Account(from_label.clone())),
				to: Some(EventAccount::User(auth_subject)),
				subject_ref: subject_ref.clone(),
				initiator_client: initiator.as_str().to_owned(),
				balances: balances.clone(),
				hold_remaining: None,
				counterparty_scope: String::new(),
				failure_reason: String::new(),
				paid_from: String::new(),
				audience: audience.clone(),
			})
		})
		.collect()
}

async fn hold_event(pool: &PgPool, ledger: &dyn Ledger, event: HoldEvent) -> Result<NewMoneyEvent, MoneyEventError> {
	let hold_id = match &event {
		HoldEvent::Placed { hold_id, .. } | HoldEvent::Captured { hold_id, .. } | HoldEvent::Released { hold_id, .. } => *hold_id,
	};
	let (client, auth_subject, hold_ref): (String, String, String) = sqlx::query_as("SELECT client_id, auth_subject, subject_ref FROM scope_holds WHERE id = $1")
		.bind(hold_id.raw())
		.fetch_one(pool)
		.await
		.map_err(|e| MoneyEventError::db(format!("scope hold {hold_id}"), e))?;
	let placer = ClientId::parse(&client).map_err(|e| MoneyEventError::corrupt(format!("scope hold {hold_id}: {e}")))?;
	let base = |scope: ScopeId, kind: MoneyEventKind, version: u64| NewMoneyEvent {
		event_id: event_id(&format!("hold:{hold_id}:{version}")),
		scope,
		kind,
		ref_id: hold_id.to_string(),
		aggregate_version: version,
		auth_subject: auth_subject.clone(),
		amount: None,
		from: None,
		to: None,
		subject_ref: String::new(),
		initiator_client: placer.as_str().to_owned(),
		balances: Vec::new(),
		hold_remaining: None,
		counterparty_scope: String::new(),
		failure_reason: String::new(),
		paid_from: String::new(),
		audience: vec![placer.clone()],
	};
	Ok(match event {
		HoldEvent::Placed { scope, amount, version, .. } => NewMoneyEvent {
			amount: Some(amount),
			subject_ref: hold_ref,
			hold_remaining: Some(amount),
			..base(scope, MoneyEventKind::HoldPlaced, version)
		},
		HoldEvent::Captured {
			scope,
			version,
			held,
			amount,
			to,
			subject_ref,
			..
		} => {
			let mut captured = base(scope.clone(), MoneyEventKind::HoldCaptured, version);
			match to {
				HoldDestination::Account(label) => {
					captured.balances.extend(balance_of(pool, ledger, &scope, &label).await?);
					captured.to = Some(EventAccount::Account(label));
				}
				HoldDestination::User(user) => captured.to = Some(EventAccount::User(auth_subject_of(pool, user).await?)),
			}
			captured.audience.extend(captured.balances.iter().map(|entry| entry.owner.clone()));
			NewMoneyEvent {
				amount: Some(amount),
				subject_ref: subject_ref.as_str().to_owned(),
				hold_remaining: Some(
					held.checked_sub(amount)
						.ok_or_else(|| MoneyEventError::corrupt(format!("scope hold {hold_id}: captured more than held")))?,
				),
				..captured
			}
		}
		HoldEvent::Released {
			scope,
			version,
			held,
			expired,
			subject_ref,
			..
		} => {
			let kind = if expired { MoneyEventKind::HoldExpired } else { MoneyEventKind::HoldVoided };
			let released = base(scope, kind, version);
			NewMoneyEvent {
				amount: Some(held),
				subject_ref: subject_ref.map(|subject| subject.as_str().to_owned()).unwrap_or_default(),
				hold_remaining: Some(Usdt::ZERO),
				// A TTL release is banking's own doing; the hold's client still receives it.
				initiator_client: if expired { String::new() } else { released.initiator_client.clone() },
				..released
			}
		}
	})
}

/// HOLD_VOIDED for a hold whose reservation the ledger refused, written as the relay parks
/// it: the money was never held, so the client must not ship on the Reserve's answer.
/// Banking's own doing, so no client is its initiator; only the hold's client receives it.
pub fn hold_refused(hold_id: Uuid, scope: ScopeId, client: ClientId, auth_subject: String, held: Usdt, version: u64) -> NewMoneyEvent {
	NewMoneyEvent {
		event_id: event_id(&format!("hold:{hold_id}:{version}")),
		scope,
		kind: MoneyEventKind::HoldVoided,
		ref_id: hold_id.to_string(),
		aggregate_version: version,
		auth_subject,
		amount: Some(held),
		from: None,
		to: None,
		subject_ref: String::new(),
		initiator_client: String::new(),
		balances: Vec::new(),
		hold_remaining: Some(Usdt::ZERO),
		counterparty_scope: String::new(),
		failure_reason: "ledger_refused".to_owned(),
		paid_from: String::new(),
		audience: vec![client],
	}
}

/// The event of an invoice's control-plane step — its creation, cancellation or expiry, as it
/// stands right after the step — for the client that issued it. Written in the step's own
/// transaction: no money moves, so there is no ledger to wait for. The payment's event is the
/// relay's ([`invoice_paid`]).
pub fn invoice_step(invoice: &Invoice) -> Result<NewMoneyEvent, DomainError> {
	let terms = invoice.terms();
	let (kind, initiator) = match invoice.state() {
		InvoiceState::Open => (MoneyEventKind::InvoiceCreated, terms.client.as_str()),
		InvoiceState::Cancelled => (MoneyEventKind::InvoiceCancelled, terms.client.as_str()),
		// Banking's own doing, by the clock the client set.
		InvoiceState::Expired => (MoneyEventKind::InvoiceExpired, ""),
		InvoiceState::Paid => return Err(DomainError::Repository(format!("invoice {}: a payment's event is the relay's", invoice.id()))),
	};
	Ok(NewMoneyEvent {
		event_id: event_id(&format!("invoice:{}:{}", invoice.id(), invoice.version())),
		scope: terms.scope.clone(),
		kind,
		ref_id: invoice.id().to_string(),
		aggregate_version: invoice.version(),
		auth_subject: String::new(),
		amount: Some(terms.amount),
		from: None,
		to: Some(EventAccount::Account(terms.to.clone())),
		subject_ref: terms.subject_ref.as_str().to_owned(),
		initiator_client: initiator.to_owned(),
		balances: Vec::new(),
		hold_remaining: None,
		counterparty_scope: String::new(),
		failure_reason: String::new(),
		paid_from: String::new(),
		audience: vec![terms.client.clone()],
	})
}

/// A payment the ledger just took, for the issuing client and the owner of the account it paid
/// into (the same client: an invoice only pays into an account its issuer owns). The payer
/// caused it, so no client is its initiator; the payer is named by `auth_subject`.
async fn invoice_paid(pool: &PgPool, ledger: &dyn Ledger, event: InvoiceEvent) -> Result<Option<NewMoneyEvent>, MoneyEventError> {
	let InvoiceEvent::Paid {
		invoice_id,
		scope,
		to_label,
		amount,
		from,
		version,
		..
	} = event
	else {
		// Only a payment is relayed; the other steps wrote their events themselves.
		return Ok(None);
	};
	let (client, subject_ref, payer): (String, String, Option<String>) = sqlx::query_as("SELECT client_id, subject_ref, paid_by_auth_subject FROM scope_invoices WHERE id = $1")
		.bind(invoice_id.raw())
		.fetch_one(pool)
		.await
		.map_err(|e| MoneyEventError::db(format!("scope invoice {invoice_id}"), e))?;
	let issuer = ClientId::parse(&client).map_err(|e| MoneyEventError::corrupt(format!("scope invoice {invoice_id}: {e}")))?;
	let payer = payer.ok_or_else(|| MoneyEventError::corrupt(format!("scope invoice {invoice_id}: relayed a payment of an unpaid invoice")))?;
	let balances: Vec<EventBalance> = balance_of(pool, ledger, &scope, &to_label).await?.into_iter().collect();
	let mut audience = vec![issuer];
	audience.extend(balances.iter().map(|entry| entry.owner.clone()));
	Ok(Some(NewMoneyEvent {
		event_id: event_id(&format!("invoice:{invoice_id}:{version}")),
		scope,
		kind: MoneyEventKind::InvoicePaid,
		ref_id: invoice_id.to_string(),
		aggregate_version: version,
		auth_subject: payer,
		amount: Some(amount),
		from: None,
		to: Some(EventAccount::Account(to_label)),
		subject_ref,
		initiator_client: String::new(),
		balances,
		hold_remaining: None,
		counterparty_scope: String::new(),
		failure_reason: String::new(),
		paid_from: from.as_str().to_owned(),
		audience,
	}))
}

/// KIND_CHARGED for a charge the ledger just took: the charging client is told, with the
/// credited account's balance after it (the client owns that account). The user's compartment
/// never appears; the client named the user, so `auth_subject` is theirs to know.
async fn charged(pool: &PgPool, ledger: &dyn Ledger, event: ChargeEvent) -> Result<NewMoneyEvent, MoneyEventError> {
	let ChargeEvent::Charged {
		charge_id, scope, to_label, amount, ..
	} = event;
	let (client, subject_ref, auth_subject): (String, String, String) = sqlx::query_as("SELECT client_id, subject_ref, auth_subject FROM scope_charges WHERE id = $1")
		.bind(charge_id.raw())
		.fetch_one(pool)
		.await
		.map_err(|e| MoneyEventError::db(format!("scope charge {charge_id}"), e))?;
	let initiator = ClientId::parse(&client).map_err(|e| MoneyEventError::corrupt(format!("scope charge {charge_id}: {e}")))?;
	let balances: Vec<EventBalance> = balance_of(pool, ledger, &scope, &to_label).await?.into_iter().collect();
	let mut audience = vec![initiator.clone()];
	audience.extend(balances.iter().map(|entry| entry.owner.clone()));
	Ok(NewMoneyEvent {
		event_id: event_id(&format!("charged:{charge_id}")),
		scope,
		kind: MoneyEventKind::Charged,
		ref_id: charge_id.to_string(),
		aggregate_version: 1,
		auth_subject,
		amount: Some(amount),
		from: None,
		to: Some(EventAccount::Account(to_label)),
		subject_ref,
		initiator_client: initiator.as_str().to_owned(),
		balances,
		hold_remaining: None,
		counterparty_scope: String::new(),
		failure_reason: String::new(),
		paid_from: String::new(),
		audience,
	})
}

fn amount_column(raw: &str, what: &str) -> Result<Option<Usdt>, DomainError> {
	if raw.is_empty() {
		return Ok(None);
	}
	raw.parse::<u128>()
		.map(|units| Some(Usdt::from_base_units(units)))
		.map_err(|_| DomainError::Repository(format!("money event {what} is not a base-unit amount: {raw:?}")))
}

fn event_account(label: Option<String>, subject: Option<String>) -> Result<Option<EventAccount>, DomainError> {
	match (label, subject) {
		(Some(label), _) => Ok(Some(EventAccount::Account(ScopeAccountLabel::parse(&label)?))),
		(None, Some(subject)) => Ok(Some(EventAccount::User(subject))),
		(None, None) => Ok(None),
	}
}

fn money_event(row: PgRow) -> Result<MoneyEventRecord, DomainError> {
	let position: i64 = row.try_get("position").map_err(repo_err)?;
	let corrupt = |e: DomainError| DomainError::Repository(format!("money event at {position}: {e}"));
	let get_text = |column: &str| row.try_get::<String, _>(column).map_err(repo_err);
	let balances: Vec<StoredBalance> = serde_json::from_str(&get_text("balances")?).map_err(|e| DomainError::Repository(format!("money event at {position}: balances: {e}")))?;
	let balances = balances
		.into_iter()
		.map(|entry| {
			Ok(EventBalance {
				label: ScopeAccountLabel::parse(&entry.label)?,
				owner: ClientId::parse(&entry.owner)?,
				balance: amount_column(&entry.balance, "balance")?.unwrap_or(Usdt::ZERO),
			})
		})
		.collect::<Result<Vec<_>, DomainError>>()
		.map_err(corrupt)?;
	let version: i64 = row.try_get("aggregate_version").map_err(repo_err)?;
	Ok(MoneyEventRecord {
		position,
		event_id: row.try_get("event_id").map_err(repo_err)?,
		scope: ScopeId::parse(&get_text("scope")?).map_err(corrupt)?,
		kind: MoneyEventKind::parse(&get_text("kind")?).map_err(corrupt)?,
		ref_id: get_text("ref_id")?,
		aggregate_version: u64::try_from(version).map_err(|_| corrupt(DomainError::Repository(format!("negative version {version}"))))?,
		auth_subject: get_text("auth_subject")?,
		amount: amount_column(&get_text("amount")?, "amount")?,
		from: event_account(row.try_get("from_label").map_err(repo_err)?, row.try_get("from_subject").map_err(repo_err)?).map_err(corrupt)?,
		to: event_account(row.try_get("to_label").map_err(repo_err)?, row.try_get("to_subject").map_err(repo_err)?).map_err(corrupt)?,
		subject_ref: get_text("subject_ref")?,
		initiator_client: get_text("initiator_client")?,
		balances,
		hold_remaining: amount_column(&row.try_get::<Option<String>, _>("hold_remaining").map_err(repo_err)?.unwrap_or_default(), "hold_remaining")?,
		counterparty_scope: get_text("counterparty_scope")?,
		failure_reason: get_text("failure_reason")?,
		paid_from: get_text("paid_from")?,
		occurred_at: row.try_get("occurred_at").map_err(repo_err)?,
	})
}

#[async_trait]
impl MoneyEventStore for PgScopeLedger {
	async fn pull(&self, client: &ClientId, scopes: &[ScopeId], after: i64, limit: u32) -> Result<Vec<MoneyEventRecord>, DomainError> {
		let scopes: Vec<&str> = scopes.iter().map(ScopeId::as_str).collect();
		sqlx::query(
			"SELECT e.position, e.event_id, e.scope, e.kind, e.ref_id, e.aggregate_version, e.auth_subject, e.amount, e.from_label, e.from_subject, e.to_label, \
			 e.to_subject, e.subject_ref, e.initiator_client, e.balances::text AS balances, e.hold_remaining, e.counterparty_scope, e.failure_reason, e.paid_from, \
			 EXTRACT(EPOCH FROM e.occurred_at)::BIGINT AS occurred_at \
			 FROM money_event_deliveries d JOIN money_events e ON e.position = d.position \
			 WHERE d.client_id = $1 AND d.position > $2 AND d.scope = ANY($3) ORDER BY d.position LIMIT $4",
		)
		.bind(client.as_str())
		.bind(after)
		.bind(&scopes)
		.bind(i64::from(limit))
		.fetch_all(self.pool())
		.await
		.map_err(repo_err)?
		.into_iter()
		.map(money_event)
		.collect()
	}
}
