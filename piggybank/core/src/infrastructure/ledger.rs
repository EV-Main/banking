//! TigerBeetle ledger gateway — the [`Ledger`] adapter.
//!
//! Translates the domain chart of accounts into TigerBeetle `Account`/`Transfer`
//! rows: resolves a [`LedgerAccountKey`] to a `u128` account id (minted once and
//! stored in the `tb_accounts` control-plane map), creates the account with the
//! correct **non-negative flag on first touch** (flags are immutable in TB), and
//! issues posted/pending transfers with **explicit amounts** and **caller-assigned
//! deterministic ids** (a re-submit returns `Exists` ⇒ idempotent). The `tb_accounts`
//! map is the adapter's own concern, read/written outside any command transaction —
//! which is exactly why [`Ledger`] is a `Gateway` (it can't join a `UnitOfWork`).

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use domain::{
	architecture::Gateway,
	balance::{LedgerAccountKey, Normal},
};
use sqlx::{PgConnection, PgPool};
use tigerbeetle as tb;
use uuid::Uuid;

use crate::{
	infrastructure::{claims::TxReads, tigerbeetle::TigerBeetle},
	ports::ledger::{CashInvariant, CashSide, CompletionKind, Ledger, LedgerBalance, LedgerError, LedgerTransfer, LinkedLeg, LockedReads, PendingCompletion},
};

/// The USDT cash ledger id (`Ledger::Usdt`) — the plane `cash_invariant` scans; each
/// row's `code` then says which side of the invariant it counts on (`CashSide`).
const USDT_LEDGER_ID: i32 = 1;

/// The TB client caps one `lookup_accounts` request at this many events; a larger request
/// resolves to `Err(TooMuchData)` and returns none of the accounts (the client only merges
/// multiple requests, it never splits one oversized one). `cash_invariant` chunks its id
/// set by this so the global conservation check survives past the cap (see `cash_invariant`).
const LOOKUP_ACCOUNTS_MAX: usize = 8189;

/// Application-level deadline on a single TigerBeetle call. The TB client surfaces a
/// *closed* connection, but a replica that accepts the connection yet stalls has no
/// in-band failure — without this bound it would block the relay's single drain loop at an
/// `.await` forever, wedging all money movement. Elapsing maps to [`LedgerError::Unavailable`],
/// which the relay treats as an unbounded retry — safe because every transfer id is
/// deterministic, so a re-submit after recovery returns `Exists`.
const TB_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Deadline for the `cash_invariant` bulk-read. Unlike hot-path calls (post, reserve,
/// complete, balance) which each touch one or two accounts, the cash-plane scan reads
/// every USDT-ledger account in chunks of [`LOOKUP_ACCOUNTS_MAX`]. On a Raspberry Pi
/// with many user-claim accounts, one chunk's `lookup_accounts` can legitimately exceed
/// [`TB_CALL_TIMEOUT`] — especially during sweep/compaction. This longer bound keeps
/// the conservation check alive at scale without touching the hot-path timeout.
const TB_CASH_INVARIANT_TIMEOUT: Duration = Duration::from_secs(30);

/// The TigerBeetle-backed [`Ledger`]. Holds the shared TB client and a Postgres pool
/// for the `tb_accounts` id-map — and, for [`Ledger::spendable`], the debits recorded but
/// not yet applied, which only the control plane knows of yet.
pub struct TbLedger {
	tb: Arc<TigerBeetle>,
	pool: PgPool,
}

impl TbLedger {
	pub fn new(tb: Arc<TigerBeetle>, pool: PgPool) -> Self {
		Self { tb, pool }
	}

	/// The balance of `key` whose id the map gave as `id`; `None` — never touched — is zero.
	async fn balance_of(&self, key: &LedgerAccountKey, id: Option<u128>) -> Result<LedgerBalance, LedgerError> {
		let Some(id) = id else {
			return Ok(LedgerBalance { posted: 0, pending: 0, locked: 0 });
		};
		let call = self
			.tb
			.client()
			.lookup_accounts(&[id])
			.map_err(|e| LedgerError::Unavailable(format!("tigerbeetle closed: {e:?}")))?;
		let accounts = deadline("lookup_accounts", call, TB_CALL_TIMEOUT)
			.await?
			.map_err(|e| LedgerError::Unavailable(format!("lookup_accounts: {e:?}")))?;
		match accounts.first() {
			Some(account) => normalized(key, account),
			None => Ok(LedgerBalance { posted: 0, pending: 0, locked: 0 }),
		}
	}

	/// Look up an existing account id from the map (no allocation).
	async fn resolve_id(&self, logical_key: &str) -> Result<Option<u128>, LedgerError> {
		let row = sqlx::query_scalar::<_, Vec<u8>>("SELECT tb_account_id FROM tb_accounts WHERE logical_key = $1")
			.bind(logical_key)
			.fetch_optional(&self.pool)
			.await
			.map_err(|e| LedgerError::Unavailable(format!("account map read: {e}")))?;
		row.map(|bytes| u128_from_be(&bytes)).transpose()
	}

	/// Resolve the id of an *existing* mapped key and assert its persisted derivation
	/// (`ledger`/`code`/`flags`) still matches what `key` derives today. TB flags are
	/// immutable on first create, so a drifted derivation would make every later create
	/// for this key a conflict that parks silently — this turns that latent runtime
	/// foot-gun into a loud, deterministic [`LedgerError::Conflict`] surfaced on the
	/// first touch after the change.
	async fn resolve_and_verify(&self, key: &LedgerAccountKey, logical_key: &str) -> Result<Option<u128>, LedgerError> {
		let row = sqlx::query_as::<_, (Vec<u8>, i32, i32, i32)>("SELECT tb_account_id, ledger, code, flags FROM tb_accounts WHERE logical_key = $1")
			.bind(logical_key)
			.fetch_optional(&self.pool)
			.await
			.map_err(|e| LedgerError::Unavailable(format!("account map read: {e}")))?;
		let Some((bytes, ledger, code, flags)) = row else {
			return Ok(None);
		};
		let stored = (ledger, code, flags);
		let derived = (key.ledger().id() as i32, key.account_code().code() as i32, account_flags(key.normal()).bits() as i32);
		if stored != derived {
			return Err(LedgerError::Conflict(format!(
				"ledger derivation drift for {logical_key}: stored (ledger,code,flags)={stored:?} != derived {derived:?}"
			)));
		}
		u128_from_be(&bytes).map(Some)
	}

	/// Resolve the account id for `key`, minting and recording a fresh `u128` on
	/// first use. Idempotent and race-safe (`ON CONFLICT DO NOTHING` + re-read),
	/// mirroring [`PgUsers::provision`](crate::infrastructure::users). For an existing
	/// row it also asserts the persisted derivation has not drifted (see
	/// [`resolve_and_verify`](Self::resolve_and_verify)).
	async fn resolve_or_allocate(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		let logical_key = key.logical_key();
		if let Some(id) = self.resolve_and_verify(key, &logical_key).await? {
			return Ok(id);
		}
		let fresh = Uuid::new_v4().as_u128();
		let bytes = fresh.to_be_bytes();
		let inserted = sqlx::query_scalar::<_, Vec<u8>>(
			"INSERT INTO tb_accounts (logical_key, tb_account_id, ledger, code, network, flags) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (logical_key) DO NOTHING RETURNING tb_account_id",
		)
		.bind(&logical_key)
		.bind(&bytes[..])
		.bind(key.ledger().id() as i32)
		.bind(key.account_code().code() as i32)
		.bind(key.network().map(|n| n.as_str()))
		.bind(account_flags(key.normal()).bits() as i32)
		.fetch_optional(&self.pool)
		.await
		.map_err(|e| LedgerError::Unavailable(format!("account map insert: {e}")))?;
		match inserted {
			Some(bytes) => u128_from_be(&bytes),
			// Lost the race — another writer inserted the same logical key first.
			None => self
				.resolve_and_verify(key, &logical_key)
				.await?
				.ok_or_else(|| LedgerError::Unavailable("account map race".into())),
		}
	}

	/// Resolve the id for `key` and ensure its TigerBeetle account exists (idempotent;
	/// always safe to call before a transfer, since the map insert and the TB create
	/// are not one atomic step).
	async fn ensure(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		let id = self.resolve_or_allocate(key).await?;
		self.create_accounts(&[tb_account(id, key)]).await?;
		Ok(id)
	}

	/// Resolve + create both legs of a transfer in one account batch.
	async fn ensure_pair(&self, debit: &LedgerAccountKey, credit: &LedgerAccountKey) -> Result<(u128, u128), LedgerError> {
		let debit_id = self.resolve_or_allocate(debit).await?;
		let credit_id = self.resolve_or_allocate(credit).await?;
		self.create_accounts(&[tb_account(debit_id, debit), tb_account(credit_id, credit)]).await?;
		Ok((debit_id, credit_id))
	}

	async fn create_accounts(&self, accounts: &[tb::Account]) -> Result<(), LedgerError> {
		let call = self
			.tb
			.client()
			.create_accounts(accounts)
			.map_err(|e| LedgerError::Unavailable(format!("tigerbeetle closed: {e:?}")))?;
		let results = deadline("create_accounts", call, TB_CALL_TIMEOUT)
			.await?
			.map_err(|e| LedgerError::Unavailable(format!("create_accounts: {e:?}")))?;
		for result in results {
			match result.status {
				// `Created` (new) and `Exists` (same id+fields) are both success.
				tb::CreateAccountStatus::Created | tb::CreateAccountStatus::Exists => {}
				other => return Err(LedgerError::Conflict(format!("create account: {other:?}"))),
			}
		}
		Ok(())
	}

	async fn create_transfers(&self, transfers: &[tb::Transfer]) -> Result<(), LedgerError> {
		let call = self
			.tb
			.client()
			.create_transfers(transfers)
			.map_err(|e| LedgerError::Unavailable(format!("tigerbeetle closed: {e:?}")))?;
		let results = deadline("create_transfers", call, TB_CALL_TIMEOUT)
			.await?
			.map_err(|e| LedgerError::Unavailable(format!("create_transfers: {e:?}")))?;
		// A linked chain reports the leg that failed with its own status and every other
		// leg as `LinkedEventFailed`; the cause is the one worth surfacing, so it is
		// looked for first and the generic chain status only stands in when nothing
		// more specific was reported.
		let mut chain_failed = false;
		for result in results {
			if result.status == tb::CreateTransferStatus::LinkedEventFailed {
				chain_failed = true;
				continue;
			}
			map_transfer_status(result.status)?;
		}
		if chain_failed {
			return Err(LedgerError::Conflict("linked transfer chain failed without a reported cause".into()));
		}
		Ok(())
	}
}

/// The id the map holds for `key`, read on `conn` — the connection a spend holds its
/// claim's lock on ([`LockedReads::account_id`]). Ids are minted once and never rewritten,
/// so this reads the same row [`TbLedger`] resolves through its pool.
pub async fn account_id(conn: &mut PgConnection, key: &LedgerAccountKey) -> Result<Option<u128>, LedgerError> {
	let row = sqlx::query_scalar::<_, Vec<u8>>("SELECT tb_account_id FROM tb_accounts WHERE logical_key = $1")
		.bind(key.logical_key())
		.fetch_optional(conn)
		.await
		.map_err(|e| LedgerError::Unavailable(format!("account map read: {e}")))?;
	row.map(|bytes| u128_from_be(&bytes)).transpose()
}

impl Gateway for TbLedger {}

#[async_trait]
impl Ledger for TbLedger {
	async fn ensure_account(&self, key: &LedgerAccountKey) -> Result<(), LedgerError> {
		self.ensure(key).await.map(|_| ())
	}

	async fn balance(&self, key: &LedgerAccountKey) -> Result<LedgerBalance, LedgerError> {
		let id = self.resolve_id(&key.logical_key()).await?;
		self.balance_of(key, id).await
	}

	async fn balance_locked(&self, key: &LedgerAccountKey, reads: &mut dyn LockedReads) -> Result<LedgerBalance, LedgerError> {
		let id = reads.account_id(key).await?;
		self.balance_of(key, id).await
	}

	async fn total_debits(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		let Some(id) = self.resolve_id(&key.logical_key()).await? else {
			return Ok(0);
		};
		let call = self
			.tb
			.client()
			.lookup_accounts(&[id])
			.map_err(|e| LedgerError::Unavailable(format!("tigerbeetle closed: {e:?}")))?;
		let accounts = deadline("lookup_accounts", call, TB_CALL_TIMEOUT)
			.await?
			.map_err(|e| LedgerError::Unavailable(format!("lookup_accounts: {e:?}")))?;
		Ok(accounts.first().map_or(0, |account| account.debits_posted.saturating_add(account.debits_pending)))
	}

	async fn spendable(&self, key: &LedgerAccountKey) -> Result<u128, LedgerError> {
		let mut conn = self.pool.acquire().await.map_err(|e| LedgerError::Unavailable(format!("control-plane connection: {e}")))?;
		self.spendable_locked(key, &mut TxReads::new(&mut conn)).await
	}

	async fn balances(&self, keys: &[LedgerAccountKey]) -> Result<Vec<LedgerBalance>, LedgerError> {
		let logical: Vec<String> = keys.iter().map(LedgerAccountKey::logical_key).collect();
		let rows = sqlx::query_as::<_, (String, Vec<u8>)>("SELECT logical_key, tb_account_id FROM tb_accounts WHERE logical_key = ANY($1)")
			.bind(&logical)
			.fetch_all(&self.pool)
			.await
			.map_err(|e| LedgerError::Unavailable(format!("account map read: {e}")))?;
		let mut id_of = std::collections::HashMap::with_capacity(rows.len());
		for (key, bytes) in rows {
			id_of.insert(key, u128_from_be(&bytes)?);
		}
		let ids: Vec<u128> = id_of.values().copied().collect();
		let mut found = std::collections::HashMap::with_capacity(ids.len());
		for chunk in ids.chunks(LOOKUP_ACCOUNTS_MAX) {
			let call = self
				.tb
				.client()
				.lookup_accounts(chunk)
				.map_err(|e| LedgerError::Unavailable(format!("tigerbeetle closed: {e:?}")))?;
			let accounts = deadline("lookup_accounts", call, TB_CALL_TIMEOUT)
				.await?
				.map_err(|e| LedgerError::Unavailable(format!("lookup_accounts: {e:?}")))?;
			found.extend(accounts.into_iter().map(|account| (account.id, account)));
		}
		// A key never touched (no map row) or not yet created in TB holds nothing.
		keys.iter()
			.zip(&logical)
			.map(|(key, logical)| match id_of.get(logical).and_then(|id| found.get(id)) {
				Some(account) => normalized(key, account),
				None => Ok(LedgerBalance { posted: 0, pending: 0, locked: 0 }),
			})
			.collect()
	}

	async fn transfer_exists(&self, id: u128) -> Result<bool, LedgerError> {
		let call = self
			.tb
			.client()
			.lookup_transfers(&[id])
			.map_err(|e| LedgerError::Unavailable(format!("tigerbeetle closed: {e:?}")))?;
		let transfers = deadline("lookup_transfers", call, TB_CALL_TIMEOUT)
			.await?
			.map_err(|e| LedgerError::Unavailable(format!("lookup_transfers: {e:?}")))?;
		Ok(!transfers.is_empty())
	}

	async fn post(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError> {
		let (debit_id, credit_id) = self.ensure_pair(&transfer.debit, &transfer.credit).await?;
		let row = tb::Transfer {
			id: transfer.id,
			debit_account_id: debit_id,
			credit_account_id: credit_id,
			amount: transfer.amount,
			ledger: transfer.debit.ledger().id(),
			code: transfer.code.code(),
			user_data_128: transfer.reference,
			..Default::default()
		};
		self.create_transfers(&[row]).await
	}

	async fn post_linked(&self, transfers: &[LedgerTransfer]) -> Result<(), LedgerError> {
		let Some(first) = transfers.first() else {
			return Ok(());
		};
		// The chain is atomic, so its first id existing means every leg already landed;
		// re-submitting would make TB answer `Exists` on the first leg and fail the rest
		// of the chain on it, which the caller could not tell from a genuine failure.
		if self.transfer_exists(first.id).await? {
			return Ok(());
		}
		let mut rows = Vec::with_capacity(transfers.len());
		for (index, transfer) in transfers.iter().enumerate() {
			let (debit_id, credit_id) = self.ensure_pair(&transfer.debit, &transfer.credit).await?;
			// Every leg but the last carries `Linked`: the flag says "and the next one",
			// so the chain closes on the first leg without it.
			let flags = if index + 1 < transfers.len() {
				tb::TransferFlags::Linked
			} else {
				tb::TransferFlags::empty()
			};
			rows.push(tb::Transfer {
				id: transfer.id,
				debit_account_id: debit_id,
				credit_account_id: credit_id,
				amount: transfer.amount,
				ledger: transfer.debit.ledger().id(),
				code: transfer.code.code(),
				user_data_128: transfer.reference,
				flags,
				..Default::default()
			});
		}
		self.create_transfers(&rows).await
	}

	async fn reserve(&self, transfer: &LedgerTransfer) -> Result<(), LedgerError> {
		let (debit_id, credit_id) = self.ensure_pair(&transfer.debit, &transfer.credit).await?;
		let row = tb::Transfer {
			id: transfer.id,
			debit_account_id: debit_id,
			credit_account_id: credit_id,
			amount: transfer.amount,
			ledger: transfer.debit.ledger().id(),
			code: transfer.code.code(),
			user_data_128: transfer.reference,
			// `timeout = 0`: the saga owns the lifecycle, not TB's clock — a pending
			// can never auto-void out from under a settle/cancel.
			flags: tb::TransferFlags::Pending,
			..Default::default()
		};
		self.create_transfers(&[row]).await
	}

	async fn complete(&self, completion: &PendingCompletion) -> Result<(), LedgerError> {
		let (debit_id, credit_id) = self.ensure_pair(&completion.debit, &completion.credit).await?;
		let (flags, amount) = match completion.kind {
			// Post the full reserved amount; void ignores amount (TB requires 0).
			CompletionKind::Post => (tb::TransferFlags::PostPendingTransfer, completion.amount),
			CompletionKind::Void => (tb::TransferFlags::VoidPendingTransfer, 0),
		};
		let row = tb::Transfer {
			id: completion.id,
			debit_account_id: debit_id,
			credit_account_id: credit_id,
			amount,
			pending_id: completion.pending_id,
			ledger: completion.debit.ledger().id(),
			code: completion.code.code(),
			user_data_128: completion.reference,
			flags,
			..Default::default()
		};
		self.create_transfers(&[row]).await
	}

	async fn apply_linked(&self, legs: &[LinkedLeg]) -> Result<(), LedgerError> {
		let Some(first) = legs.first() else {
			return Ok(());
		};
		// As in `post_linked`: the chain is atomic, so its first id existing means it landed.
		if self.transfer_exists(first.id()).await? {
			return Ok(());
		}
		let mut rows = Vec::with_capacity(legs.len());
		for (index, leg) in legs.iter().enumerate() {
			let linked = if index + 1 < legs.len() { tb::TransferFlags::Linked } else { tb::TransferFlags::empty() };
			let row = match leg {
				LinkedLeg::Post(transfer) | LinkedLeg::Reserve(transfer) => {
					let (debit_id, credit_id) = self.ensure_pair(&transfer.debit, &transfer.credit).await?;
					// `timeout = 0` on a reserve, as in `reserve`: the saga owns its lifecycle.
					let kind = if matches!(leg, LinkedLeg::Reserve(_)) {
						tb::TransferFlags::Pending
					} else {
						tb::TransferFlags::empty()
					};
					tb::Transfer {
						id: transfer.id,
						debit_account_id: debit_id,
						credit_account_id: credit_id,
						amount: transfer.amount,
						ledger: transfer.debit.ledger().id(),
						code: transfer.code.code(),
						user_data_128: transfer.reference,
						flags: kind | linked,
						..Default::default()
					}
				}
				LinkedLeg::Complete(completion) => {
					let (debit_id, credit_id) = self.ensure_pair(&completion.debit, &completion.credit).await?;
					let (kind, amount) = match completion.kind {
						CompletionKind::Post => (tb::TransferFlags::PostPendingTransfer, completion.amount),
						CompletionKind::Void => (tb::TransferFlags::VoidPendingTransfer, 0),
					};
					tb::Transfer {
						id: completion.id,
						debit_account_id: debit_id,
						credit_account_id: credit_id,
						amount,
						pending_id: completion.pending_id,
						ledger: completion.debit.ledger().id(),
						code: completion.code.code(),
						user_data_128: completion.reference,
						flags: kind | linked,
						..Default::default()
					}
				}
			};
			rows.push(row);
		}
		self.create_transfers(&rows).await
	}

	async fn cash_invariant(&self) -> Result<CashInvariant, LedgerError> {
		// Every cash-plane account id lives in the id-map; pull the USDT-ledger rows with
		// their code so we can split custody (wallet) from claims, then read the posted
		// balances straight from TB (authoritative) and sum each side on its natural side.
		// Keyed by the row's `code`, not by parsing its key: a row written under a key
		// format this build no longer spells (the per-rail claims migration `0004` left
		// behind) still carries the kind it was created with, and the conservation check
		// must count it rather than fail on it.
		let rows = sqlx::query_as::<_, (Vec<u8>, i32)>("SELECT tb_account_id, code FROM tb_accounts WHERE ledger = $1")
			.bind(USDT_LEDGER_ID)
			.fetch_all(&self.pool)
			.await
			.map_err(|e| LedgerError::Unavailable(format!("cash-plane account scan: {e}")))?;
		if rows.is_empty() {
			return Ok(CashInvariant::default());
		}
		let mut ids = Vec::with_capacity(rows.len());
		let mut side_of = std::collections::HashMap::with_capacity(rows.len());
		for (bytes, code) in &rows {
			let id = u128_from_be(bytes)?;
			ids.push(id);
			let side = u16::try_from(*code).ok().and_then(CashSide::of);
			side_of.insert(id, side);
		}
		// One `lookup_accounts` is capped at `LOOKUP_ACCOUNTS_MAX`; the cash plane grows
		// one `UserClaim` account per user, so at scale this id set exceeds the cap. Chunk
		// it and accumulate each side across pages — an unchunked read would fail wholesale
		// and silently disable the only global conservation check.
		let mut inv = CashInvariant::default();
		for chunk in ids.chunks(LOOKUP_ACCOUNTS_MAX) {
			let call = self
				.tb
				.client()
				.lookup_accounts(chunk)
				.map_err(|e| LedgerError::Unavailable(format!("tigerbeetle closed: {e:?}")))?;
			let accounts = deadline("lookup_accounts", call, TB_CASH_INVARIANT_TIMEOUT)
				.await?
				.map_err(|e| LedgerError::Unavailable(format!("lookup_accounts: {e:?}")))?;
			for account in accounts {
				let side = side_of.get(&account.id).copied().flatten();
				if side == Some(CashSide::Custody) {
					// Custody is debit-normal: posted = debits − credits.
					inv.custody = inv.custody.saturating_add(account.debits_posted.saturating_sub(account.credits_posted));
					continue;
				}
				// Claims are credit-normal: posted = credits − debits. Every non-custody
				// account is a claim for the global check; only the known kinds are also
				// attributed, so an account of an unknown kind shows up as unclassified.
				let posted = account.credits_posted.saturating_sub(account.debits_posted);
				inv.claims = inv.claims.saturating_add(posted);
				let bucket = match side {
					Some(CashSide::UserClaims) => &mut inv.user_claims,
					Some(CashSide::Clearing) => &mut inv.clearing,
					Some(CashSide::ScopeClaims) => &mut inv.scope_claims,
					Some(CashSide::ScopeAccounts) => &mut inv.scope_accounts,
					Some(CashSide::RetiredClaims) => &mut inv.retired_claims,
					Some(CashSide::Custody) | None => continue,
				};
				*bucket = bucket.saturating_add(posted);
			}
		}
		Ok(inv)
	}
}

/// Ensure the singleton accounts exist at boot: a custody wallet per network (the
/// per-rail treasury), the network-agnostic withdrawal-clearing account and the mocked
/// bank custody. Per-user/-scope claim accounts are created lazily on first transfer.
///
/// No live key names a retired account; production still has their rows in `tb_accounts`,
/// and the cash scan classifies them by code.
pub async fn seed_singletons(ledger: &dyn Ledger) -> Result<(), LedgerError> {
	use domain::money::Network;
	for network in Network::ALL {
		ledger.ensure_account(&LedgerAccountKey::CryptoWallet(network)).await?;
	}
	ledger.ensure_account(&LedgerAccountKey::WithdrawalClearing).await?;
	ledger.ensure_account(&LedgerAccountKey::BankCustody).await?;
	Ok(())
}

/// An account's balance on its natural side. `posted` is a real non-negative invariant (the
/// TB flag guarantees it), so an underflow there is a genuine ledger inconsistency.
/// `pending` (in-flight inflow) and `locked` (in-flight outflow reserved against this
/// account) are the two sides of a pending and are legitimately one-sided, so they
/// saturate to zero rather than error.
fn normalized(key: &LedgerAccountKey, account: &tb::Account) -> Result<LedgerBalance, LedgerError> {
	let (posted, pending, locked) = match key.normal() {
		Normal::Credit => (
			account.credits_posted.checked_sub(account.debits_posted),
			account.credits_pending.saturating_sub(account.debits_pending),
			account.debits_pending.saturating_sub(account.credits_pending),
		),
		Normal::Debit => (
			account.debits_posted.checked_sub(account.credits_posted),
			account.debits_pending.saturating_sub(account.credits_pending),
			account.credits_pending.saturating_sub(account.debits_pending),
		),
	};
	let posted = posted.ok_or_else(|| LedgerError::Conflict(format!("balance underflow on {}", key.logical_key())))?;
	Ok(LedgerBalance { posted, pending, locked })
}

/// Map a single TigerBeetle transfer status to a [`LedgerError`]. The idempotent-
/// success set is `Created | Exists | AlreadyPosted | AlreadyVoided`; a post racing
/// its pending create is `Retryable`; a non-negative violation is `InsufficientFunds`;
/// everything else is a genuine inconsistency to surface, never absorb.
fn map_transfer_status(status: tb::CreateTransferStatus) -> Result<(), LedgerError> {
	use tb::CreateTransferStatus as S;
	match status {
		S::Created | S::Exists | S::PendingTransferAlreadyPosted | S::PendingTransferAlreadyVoided => Ok(()),
		S::ExceedsCredits | S::ExceedsDebits => Err(LedgerError::InsufficientFunds),
		S::PendingTransferNotFound => Err(LedgerError::Retryable("pending transfer not found yet".into())),
		other => Err(LedgerError::Conflict(format!("create transfer: {other:?}"))),
	}
}

/// Bound a single in-flight TigerBeetle call by `limit`; an elapsed deadline is reported
/// as [`LedgerError::Unavailable`] (a stalled, not closed, replica), so the relay retries
/// it rather than blocking its single drain loop on the hung `.await`.
async fn deadline<F: std::future::Future>(op: &str, call: F, limit: Duration) -> Result<F::Output, LedgerError> {
	tokio::time::timeout(limit, call)
		.await
		.map_err(|_| LedgerError::Unavailable(format!("{op}: timed out after {limit:?}")))
}

fn tb_account(id: u128, key: &LedgerAccountKey) -> tb::Account {
	tb::Account {
		id,
		ledger: key.ledger().id(),
		code: key.account_code().code(),
		flags: account_flags(key.normal()),
		..Default::default()
	}
}

/// The non-negative guard for an account's natural side — set once at create and
/// immutable thereafter, so a claim can never be over-spent nor custody go negative.
fn account_flags(normal: Normal) -> tb::AccountFlags {
	match normal {
		Normal::Debit => tb::AccountFlags::CreditsMustNotExceedDebits,
		Normal::Credit => tb::AccountFlags::DebitsMustNotExceedCredits,
	}
}

fn u128_from_be(bytes: &[u8]) -> Result<u128, LedgerError> {
	let array: [u8; 16] = bytes.try_into().map_err(|_| LedgerError::Conflict("malformed ledger account id".into()))?;
	Ok(u128::from_be_bytes(array))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A TigerBeetle replica that accepts the connection but stalls (never replies) leaves
	/// the relay's drain loop awaiting a future that never resolves. The deadline must turn
	/// that hang into a bounded `Unavailable` — which the relay retries — rather than wedging
	/// the single worker forever. A never-resolving future stands in for the stalled replica;
	/// the paused clock makes the 5s deadline elapse instantly.
	#[tokio::test(start_paused = true)]
	async fn a_stalled_tigerbeetle_call_elapses_to_unavailable() {
		let stalled = std::future::pending::<()>();
		let result = deadline("lookup_accounts", stalled, TB_CALL_TIMEOUT).await;
		assert!(matches!(result, Err(LedgerError::Unavailable(_))), "a hung TB call must map to Unavailable, not hang");
	}
}
