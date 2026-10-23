//! Consilium mail worker — drains `consilium_mail` into the identity plane's mailer.
//!
//! WHY THIS IS A WORKER AND NOT AN INLINE CALL. Every governance mail is written in the same
//! transaction as the fact it announces. A vote that was validly cast must not be rolled
//! back because concierge was briefly unreachable, and a vote must not be recorded with its
//! notification silently dropped. Committing both together and delivering the second half
//! afterwards is the only shape that gives both — the same argument the outbox relay makes
//! for money.
//!
//! The relay RPC is idempotent by `dedupe_key`, so redelivering is free. On success the
//! payload is rewritten **without** its secrets: the plaintext approval code exists for
//! exactly as long as it takes to hand the message over, and no longer.

use std::{sync::Arc, time::Duration};

use domain::error::DomainError;
use sqlx::{PgConnection, PgPool, Row};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::ports::governance_mail::{GovernanceMail, GovernanceMailer, MailDeliveryError};

/// What a queued mail is ABOUT — the row it announces or asks a decision on. Exactly one,
/// which `consilium_mail_names_one_subject` states to the database; the worker uses it to
/// know which seat's `notified` flag a delivered token mail flips.
#[derive(Clone, Copy)]
pub enum MailSubject {
	Consilium(Uuid),
	Payment(Uuid),
	/// A change of fee terms whose holders are being notified.
	FeePolicyChange(Uuid),
}

/// Queue one mail on the caller's open transaction, so the notification commits with the
/// fact it announces or not at all. `ON CONFLICT DO NOTHING` on the dedupe key makes a
/// retried transition (or a redelivered sweep) enqueue the same notification exactly once.
///
/// `user_id` is the recipient's BANKING id; the worker resolves it to the concierge id at
/// send time, because the relay addresses identities in the plane that owns them.
pub async fn enqueue(conn: &mut PgConnection, subject: MailSubject, user_id: Uuid, dedupe_key: &str, mail: &GovernanceMail) -> Result<(), DomainError> {
	let payload = serde_json::to_string(mail).map_err(|e| DomainError::Repository(e.to_string()))?;
	let (consilium_id, payment_id, fee_policy_change_id) = match subject {
		MailSubject::Consilium(id) => (Some(id), None, None),
		MailSubject::Payment(id) => (None, Some(id), None),
		MailSubject::FeePolicyChange(id) => (None, None, Some(id)),
	};
	sqlx::query(
		"INSERT INTO consilium_mail (consilium_id, payment_id, fee_policy_change_id, user_id, kind, dedupe_key, payload) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb) ON CONFLICT (dedupe_key) DO NOTHING",
	)
	.bind(consilium_id)
	.bind(payment_id)
	.bind(fee_policy_change_id)
	.bind(user_id)
	.bind(mail.as_str())
	.bind(dedupe_key)
	.bind(payload)
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(())
}

/// Withdraw every mail about `subject` that has not been delivered, on the caller's open
/// transaction: the fact the mails announce is being taken back, and they must go with it or
/// not at all. Terminal — a withdrawn row is never drained and never counted as a mail
/// somebody is still owed — and distinct from a row given up on: nobody failed to reach
/// anybody. A delivered mail is left as it is; the schema refuses to withdraw one. Returns
/// how many rows were withdrawn. The UPDATE takes each row's lock, which the worker holds
/// across a delivery in flight ([`ConsiliumMailer::deliver`]): this waits for that outcome
/// rather than withdrawing a mail that is being handed over.
///
/// Not for a consilium, and not for a payment. A closing consilium's queue holds its verdict
/// mail and burn notices beside the invitations, and taking back everything undelivered would
/// silence the first two; a consilium withdraws its invitations alone, through
/// [`withdraw_undelivered_invitations`]. A payment's queue holds its consent invitation,
/// which carries a token and a code — withdrawing it without blanking them would leave a live
/// credential on a row nobody will ever deliver — and, once the consent has died, the
/// outcome notice that must still reach its audience; a payment withdraws the invitation
/// alone, through [`withdraw_undelivered_consent`]. Both are refused here rather than done wrong.
pub async fn withdraw_undelivered(conn: &mut PgConnection, subject: MailSubject) -> Result<u64, DomainError> {
	let (sql, id) = match subject {
		MailSubject::Consilium(_) => {
			return Err(DomainError::Repository(
				"a consilium's undelivered mail is not withdrawn wholesale — see withdraw_undelivered_invitations".into(),
			));
		}
		MailSubject::Payment(_) => {
			return Err(DomainError::Repository(
				"a payment's undelivered consent mail is not withdrawn without redacting its secrets — see withdraw_undelivered_consent".into(),
			));
		}
		MailSubject::FeePolicyChange(id) => (
			"UPDATE consilium_mail SET withdrawn_at = now() WHERE fee_policy_change_id = $1 AND sent_at IS NULL AND withdrawn_at IS NULL",
			id,
		),
	};
	let withdrawn = sqlx::query(sql).bind(id).execute(&mut *conn).await.map_err(repo_err)?;
	Ok(withdrawn.rows_affected())
}

/// Withdraw the approval INVITATIONS of a consilium that the relay has not taken — the mails
/// carrying a seat's token and code — and nothing else about it (#342). The consilium is
/// closing; an invitation still queued behind a relay outage would otherwise reach its owner
/// later, asking for a vote nobody can cast. Only the token-bearing kinds go: the outcome mail
/// the closing enqueues right after this must reach the audience, and a burn notice already
/// queued reports a brute-force attempt that closing the consilium does not unmake. The kinds
/// are the consilium-addressed ones of [`GovernanceMail::carries_a_token`].
///
/// The secrets go with the withdrawal, as they do when a row is given up on
/// ([`ConsiliumMailer::retire`]): a token and a code that will never be delivered are a
/// credential nobody legitimately holds. The two fields blanked are the ones
/// [`GovernanceMail::redacted`] blanks for these kinds.
///
/// Safe at any point of a transition, and NOT to be called while the seats are held: the
/// worker holds an invitation's row across the relay call and, once the message is taken,
/// flips the seat's `notified` ([`ConsiliumMailer::deliver`]) — a transition holding the seats
/// while waiting on the mail row would deadlock with it. Taken BEFORE the consilium and its
/// seats are locked, this waits only for a delivery in flight to finish, and its row is then
/// a delivered one, left as it is.
pub async fn withdraw_undelivered_invitations(conn: &mut PgConnection, consilium_id: Uuid) -> Result<u64, DomainError> {
	let withdrawn = sqlx::query(
		"UPDATE consilium_mail SET withdrawn_at = now(), payload = payload || '{\"approval_url\": \"\", \"code\": \"\"}'::jsonb \
		 WHERE consilium_id = $1 AND sent_at IS NULL AND withdrawn_at IS NULL \
		 AND kind IN ('payment_approval', 'fee_policy_approval')",
	)
	.bind(consilium_id)
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(withdrawn.rows_affected())
}

/// Withdraw the consent INVITATION of a payment that the relay has not taken — the one mail
/// carrying the subject's token and code (#368). The order is closing without a verdict (its
/// operator withdrew it, or its window ran out); a consent still queued behind a relay outage
/// would otherwise reach the investor later, asking them to consent to an order nobody can
/// act on — with a token that still resolves. Only `payment_consent` goes: naming it keeps the
/// payment's `payment_outcome` notice (#238) out of this withdrawal, as
/// [`withdraw_undelivered_invitations`] keeps a consilium's verdict mail.
///
/// The secrets go with the withdrawal, as they do when a row is given up on
/// ([`ConsiliumMailer::retire`]): a token and a code that will never be delivered are a
/// credential nobody legitimately holds. The two fields blanked are the ones
/// [`GovernanceMail::redacted`] blanks for this kind.
///
/// Safe at any point of a transition, and NOT to be called while the order is held: the
/// worker holds the invitation's row across the relay call and, once the message is taken,
/// flips `payment_consent.notified` ([`ConsiliumMailer::deliver`]) — a transition holding the
/// order while waiting on the mail row would deadlock with it. Taken BEFORE the `payments`
/// row is locked, this waits only for a delivery in flight to finish, and its row is then a
/// delivered one, left as it is.
pub async fn withdraw_undelivered_consent(conn: &mut PgConnection, payment_id: Uuid) -> Result<u64, DomainError> {
	let withdrawn = sqlx::query(
		"UPDATE consilium_mail SET withdrawn_at = now(), payload = payload || '{\"approval_url\": \"\", \"code\": \"\"}'::jsonb \
		 WHERE payment_id = $1 AND sent_at IS NULL AND withdrawn_at IS NULL AND kind = 'payment_consent'",
	)
	.bind(payment_id)
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(withdrawn.rows_affected())
}

const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Rows per pass. Governance mail is low volume by nature — a handful per consilium.
const BATCH: i64 = 100;

/// After this many failures a row stops being retried and starts being an alert. It is
/// never deleted: the audit trail keeps what could not be delivered, and why. A row at the
/// ceiling with no `sent_at` is one that was GIVEN UP ON, whichever way it got there — a
/// deferral past [`DEFERRAL_CEILING`] is pinned here too.
pub(crate) const MAX_ATTEMPTS: i32 = 10;

/// How long a mail keeps being DEFERRED — the relay throttling its recipient, or being
/// unreachable — before it is finally given up on. A deferral charges no attempt (nothing
/// about the message was refused), so the ceiling here is time, not count: a day covers a
/// relay outage or a busy recipient without letting a throttled approval mail sit forever.
pub const DEFERRAL_CEILING: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest wait between two deferred retries. The backoff doubles from the sweep
/// interval and stops here, so a recipient the relay is protecting is asked again minutes
/// apart, not seconds — and a relay coming back is noticed within the hour.
const MAX_DEFERRAL_BACKOFF: Duration = Duration::from_secs(60 * 60);

pub struct ConsiliumMailer {
	pool: PgPool,
	mailer: Arc<dyn GovernanceMailer>,
}

impl ConsiliumMailer {
	pub fn new(pool: PgPool, mailer: Arc<dyn GovernanceMailer>) -> Self {
		Self { pool, mailer }
	}

	/// Run as the SINGLETON mailer.
	///
	/// The relay RPC is idempotent by `dedupe_key`, so a second mailer cannot double-send —
	/// but it can double the work, double the `attempts` charged against a struggling
	/// recipient, and walk a row to `MAX_ATTEMPTS` twice as fast during an outage, turning a
	/// recoverable concierge blip into a permanently abandoned approval mail. Multi-replica
	/// is an anticipated shape here, so the singleton is enforced rather than assumed.
	///
	/// As in the relay and the sweeper, a transient DB failure re-acquires with backoff and
	/// never returns — an early return would cancel every sibling task in the composition
	/// root's `join!`.
	pub async fn run(self, shutdown: CancellationToken) {
		const MAX_BACKOFF: Duration = Duration::from_secs(30);
		let mut backoff = Duration::from_millis(500);
		info!("consilium mailer: delivering governance mail every {SWEEP_INTERVAL:?}");
		'acquire: loop {
			let mut lock = tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				lock = super::singleton::acquire(&self.pool, super::singleton::keys::CONSILIUM_MAILER) => match lock {
					Ok(lock) => lock,
					Err(err) => {
						error!("consilium mailer: could not acquire the mailer lock (retrying in {backoff:?}): {err}");
						tokio::select! {
							() = shutdown.cancelled() => return,
							() = tokio::time::sleep(backoff) => {},
						}
						backoff = (backoff * 2).min(MAX_BACKOFF);
						continue 'acquire;
					}
				},
			};
			backoff = Duration::from_millis(500);
			info!("consilium mailer: acquired the mailer lock — delivering as the singleton worker");
			loop {
				if let Err(err) = self.drain().await {
					warn!("consilium mailer: drain failed (will retry): {err}");
				}
				tokio::select! {
					() = shutdown.cancelled() => return,
					() = tokio::time::sleep(SWEEP_INTERVAL) => {},
				}
				if !super::singleton::still_held(&mut lock).await {
					error!("consilium mailer: lost the mailer lock connection — re-acquiring");
					let _ = lock.close().await;
					continue 'acquire;
				}
			}
		}
	}

	/// One pass. Returns how many messages were handed over. Public so an integration test
	/// can drive it deterministically.
	pub async fn drain(&self) -> Result<usize, DomainError> {
		// A deferred row waits out its backoff; a withdrawn one is never due again; everything
		// else undelivered and under the ceiling is due now. Ids only: each row is then
		// re-read and LOCKED in its own transaction, so what is judged is the row of the
		// moment it is delivered, not of the moment the pass began.
		let ids: Vec<i64> = sqlx::query_scalar(
			"SELECT id FROM consilium_mail \
			 WHERE sent_at IS NULL AND withdrawn_at IS NULL AND attempts < $1 AND (next_attempt_at IS NULL OR next_attempt_at <= now()) \
			 ORDER BY id LIMIT $2",
		)
		.bind(MAX_ATTEMPTS)
		.bind(BATCH)
		.fetch_all(&self.pool)
		.await
		.map_err(repo_err)?;

		let mut sent = 0;
		for id in ids {
			if self.deliver(id).await? {
				sent += 1;
			}
		}
		Ok(sent)
	}

	/// One row, in ONE transaction that holds the row's lock from the moment it is read until
	/// its outcome is written — across the relay call. A transition that withdraws the row
	/// ([`withdraw_undelivered`], on a cancel) takes the same lock, so it either runs before
	/// this read (and the row is skipped here) or waits until the outcome is committed: a
	/// mail is never handed over after the fact it announces was taken back, and the outcome
	/// is never written over a withdrawal. Without the lock, a cancel landing between the
	/// read and the send would deliver a withdrawn notice AND then trip
	/// `consilium_mail_withdrawn_is_unsent` on the write-back, leaving a delivered mail on
	/// record as "nobody told". The relay call is bounded (connect and request deadlines on
	/// the channel), so the wait a cancel may sit out is bounded with it. `SKIP LOCKED`: a row
	/// under such a transition right now is left to the next pass, which sees its result.
	///
	/// Returns whether the message was handed over.
	async fn deliver(&self, id: i64) -> Result<bool, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query(
			"SELECT m.dedupe_key, m.payload::text AS payload, u.concierge_user_id \
			 FROM consilium_mail m JOIN users u ON u.id = m.user_id \
			 WHERE m.id = $1 AND m.sent_at IS NULL AND m.withdrawn_at IS NULL AND m.attempts < $2 AND (m.next_attempt_at IS NULL OR m.next_attempt_at <= now()) \
			 FOR UPDATE OF m SKIP LOCKED",
		)
		.bind(id)
		.bind(MAX_ATTEMPTS)
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
		// Withdrawn, delivered or locked by a transition since the pass began: not this pass's.
		let Some(row) = row else {
			return Ok(false);
		};
		let dedupe_key: String = row.try_get("dedupe_key").map_err(repo_err)?;
		let payload: String = row.try_get("payload").map_err(repo_err)?;
		let concierge_user_id: Option<Uuid> = row.try_get("concierge_user_id").map_err(repo_err)?;
		let mut mail: GovernanceMail = match serde_json::from_str(&payload) {
			Ok(mail) => mail,
			Err(err) => {
				// Nothing typed to redact: a payload this worker cannot read is one it
				// cannot strip either, and the row is retired as it stands.
				self.fail(&mut tx, id, &format!("unreadable payload: {err}"), None).await?;
				tx.commit().await.map_err(repo_err)?;
				return Ok(false);
			}
		};
		// The recipient is addressed in the plane that OWNS identities, so the money
		// plane cannot redirect a governance mail. Without the mirrored id there is no
		// safe address to send to, and guessing is not an option.
		//
		// What a missing mirror costs the row depends on the kind. A holder's NOTICE is queued
		// whether or not the holder was mirrored at the time (#325): units can be issued
		// before the holder ever signs in to the platform, and the mirror lands with their first
		// login — hours or days later. Nothing about the message was refused, so the row is
		// DEFERRED like a relay outage, charged no attempt, and the mirror landing anywhere
		// inside the notice period is enough for the next pass to reach them; past the
		// deferral ceiling (equal to the notice period) `defer` retires it with this same
		// reason, which is the row the operator's acknowledgement is then offered over. An
		// approval or a consent is charged as before: its token is bound to a subject the
		// caller had to resolve at open, so an empty mirror there is a fault worth an alert
		// within minutes, not a login still to come.
		let Some(recipient) = concierge_user_id else {
			const NO_MIRROR: &str = "recipient has no mirrored concierge user id";
			if matches!(mail, GovernanceMail::FeePolicyNotice(_)) {
				self.defer(&mut tx, id, NO_MIRROR, &mail).await?;
			} else {
				self.fail(&mut tx, id, NO_MIRROR, Some(&mail)).await?;
			}
			tx.commit().await.map_err(repo_err)?;
			return Ok(false);
		};
		// A holder's notice names its addressee in the identity plane as a consent does,
		// and concierge refuses the two disagreeing — so the name is taken from the mirror
		// of THIS moment, the same row the address above came from, never from whatever the
		// queue row was written with before the mirror existed.
		if let GovernanceMail::FeePolicyNotice(notice) = &mut mail {
			notice.subject_user_id = recipient.to_string();
		}
		let sent = match self.mailer.send(recipient, &dedupe_key, &mail).await {
			Ok(()) => {
				let redacted = serde_json::to_string(&mail.redacted()).map_err(|e| DomainError::Repository(e.to_string()))?;
				sqlx::query("UPDATE consilium_mail SET sent_at = now(), payload = $2::jsonb WHERE id = $1")
					.bind(id)
					.bind(redacted)
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
				// `notified` used to be set TRUE at INSERT, where it meant "queued" while
				// the operator screen reading it says "notified". During a concierge outage
				// every seat would show as notified with not one mail delivered — the screen
				// would look healthiest exactly when the mechanism was most broken. It is set
				// HERE, once concierge has actually taken the message, and only for a mail
				// that carries a token: an outcome or burn notice tells the recipient nothing
				// about whether they were given one to answer with. Which seat table holds
				// the flag follows from which subject the row names.
				if mail.carries_a_token() {
					sqlx::query("UPDATE consilium_voter v SET notified = TRUE FROM consilium_mail m WHERE m.id = $1 AND v.consilium_id = m.consilium_id AND v.user_id = m.user_id")
						.bind(id)
						.execute(&mut *tx)
						.await
						.map_err(repo_err)?;
					sqlx::query("UPDATE payment_consent c SET notified = TRUE FROM consilium_mail m WHERE m.id = $1 AND c.payment_id = m.payment_id AND c.subject_user_id = m.user_id")
						.bind(id)
						.execute(&mut *tx)
						.await
						.map_err(repo_err)?;
				}
				true
			}
			Err(MailDeliveryError::Deferred(why)) => {
				self.defer(&mut tx, id, &why, &mail).await?;
				false
			}
			Err(MailDeliveryError::Failed(why)) => {
				self.fail(&mut tx, id, &why, Some(&mail)).await?;
				false
			}
		};
		tx.commit().await.map_err(repo_err)?;
		Ok(sent)
	}

	/// Put a row back for later WITHOUT charging an attempt: the relay is throttling this
	/// recipient or is down, or a holder's notice has no mirror to be addressed to yet, and
	/// none of that says anything about the message. The wait doubles
	/// from the sweep interval up to [`MAX_DEFERRAL_BACKOFF`]; past [`DEFERRAL_CEILING`] from
	/// creation the row is given up on the way a failed one is, so a permanently throttled
	/// recipient still becomes an alert rather than a silent stall.
	async fn defer(&self, conn: &mut PgConnection, id: i64, reason: &str, mail: &GovernanceMail) -> Result<(), DomainError> {
		let expired: bool = sqlx::query_scalar("SELECT created_at + $2 < now() FROM consilium_mail WHERE id = $1")
			.bind(id)
			.bind(DEFERRAL_CEILING)
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		if expired {
			self.retire(conn, id, reason, Some(mail)).await?;
			error!(
				mail_id = id,
				"consilium mailer: giving up on a governance mail deferred for over {DEFERRAL_CEILING:?} — its recipient will not be told: {reason}"
			);
			return Ok(());
		}
		let deferrals: i32 = sqlx::query_scalar("UPDATE consilium_mail SET deferrals = deferrals + 1, last_error = $2 WHERE id = $1 RETURNING deferrals")
			.bind(id)
			.bind(reason)
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		let backoff = SWEEP_INTERVAL.saturating_mul(1u32 << (deferrals - 1).clamp(0, 16)).min(MAX_DEFERRAL_BACKOFF);
		sqlx::query("UPDATE consilium_mail SET next_attempt_at = now() + $2 WHERE id = $1")
			.bind(id)
			.bind(backoff)
			.execute(&mut *conn)
			.await
			.map_err(repo_err)?;
		warn!(mail_id = id, deferrals, ?backoff, "consilium mailer: delivery deferred (no attempt charged): {reason}");
		Ok(())
	}

	/// Record a delivery failure. At the ceiling it becomes an `error!` (Sentry-shipped): an
	/// approval mail that never arrives is a consilium that can never reach quorum, which is
	/// exactly the kind of silent stall an operator must be told about — and the row is
	/// retired with its secrets stripped, exactly as a delivered one is.
	async fn fail(&self, conn: &mut PgConnection, id: i64, reason: &str, mail: Option<&GovernanceMail>) -> Result<(), DomainError> {
		let attempts: i32 = sqlx::query_scalar("UPDATE consilium_mail SET attempts = attempts + 1, last_error = $2 WHERE id = $1 RETURNING attempts")
			.bind(id)
			.bind(reason)
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		if attempts >= MAX_ATTEMPTS {
			self.retire(conn, id, reason, mail).await?;
			error!(mail_id = id, attempts, "consilium mailer: giving up on a governance mail — an owner will not be told: {reason}");
		} else {
			warn!(mail_id = id, attempts, "consilium mailer: delivery failed (will retry): {reason}");
		}
		Ok(())
	}

	/// Give a row up for good: pinned at the attempt ceiling so no sweep picks it up again,
	/// and its payload rewritten WITHOUT its secrets. A token and a code that will never be
	/// delivered are a credential nobody legitimately holds, and the row is kept for the
	/// audit trail — which needs to say what was attempted, not what the code was.
	async fn retire(&self, conn: &mut PgConnection, id: i64, reason: &str, mail: Option<&GovernanceMail>) -> Result<(), DomainError> {
		let redacted = mail
			.map(|mail| serde_json::to_string(&mail.redacted()))
			.transpose()
			.map_err(|e| DomainError::Repository(e.to_string()))?;
		sqlx::query("UPDATE consilium_mail SET attempts = $2, last_error = $3, payload = COALESCE($4::jsonb, payload) WHERE id = $1")
			.bind(id)
			.bind(MAX_ATTEMPTS)
			.bind(reason)
			.bind(redacted)
			.execute(&mut *conn)
			.await
			.map_err(repo_err)?;
		Ok(())
	}
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// How many governance mails are still undelivered — the number the boot warning quotes when
/// the seam is unwired. A withdrawn mail is not owed to anybody and is not counted.
pub async fn pending_count(pool: &PgPool) -> Result<i64, DomainError> {
	sqlx::query_scalar("SELECT COUNT(*) FROM consilium_mail WHERE sent_at IS NULL AND withdrawn_at IS NULL")
		.fetch_one(pool)
		.await
		.map_err(repo_err)
}
