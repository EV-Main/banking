//! Notices to a person about their own money, through concierge's scope inbox — the outbox
//! that holds them and the worker that delivers them.
//!
//! A notice is written in the same transaction as the fact it announces, so it cannot be
//! lost to a concierge outage nor sent for a fact that rolled back, and is delivered
//! afterwards. `EmitScopeNotification` is idempotent by `dedupe_key`, so redelivery is free.
//! Unlike the governance mail it replaces on this path, a notice carries no secret: nothing
//! is redacted and nothing is blanked on withdrawal.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use domain::error::DomainError;
use evconcierge_contracts::concierge::v1::{EmitRequest, scope_inbox_service_client::ScopeInboxServiceClient};
use sqlx::{PgConnection, PgPool, Row};
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, metadata::MetadataValue, transport::Channel};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::ports::scope_notifications::{NoticeError, ScopeNotice, ScopeNotifier};

/// Queue one notice on the caller's open transaction. `ON CONFLICT DO NOTHING` on the dedupe
/// key makes a retried transition enqueue the same notice once.
pub async fn enqueue(conn: &mut PgConnection, notice: &ScopeNotice, payment: Option<Uuid>) -> Result<(), DomainError> {
	sqlx::query(
		"INSERT INTO scope_notification_outbox (user_id, topic, kind, title, body, link, dedupe_key, payment_id) \
		 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (dedupe_key) DO NOTHING",
	)
	.bind(notice.user.raw())
	.bind(&notice.topic)
	.bind(&notice.kind)
	.bind(&notice.title)
	.bind(&notice.body)
	.bind(&notice.link)
	.bind(&notice.dedupe_key)
	.bind(payment)
	.execute(&mut *conn)
	.await
	.map_err(repo_err)?;
	Ok(())
}

/// Take back the undelivered notices of `kind` about a payment: the order closed, and a
/// person reached once concierge is back would be asked to answer an order nobody can act
/// on. A delivered notice stays as it is. The UPDATE waits for a delivery in flight on the
/// same row (the worker holds it across the emit), so a notice is never withdrawn while it is
/// being handed over.
pub async fn withdraw_undelivered(conn: &mut PgConnection, payment: Uuid, kind: &str) -> Result<u64, DomainError> {
	let withdrawn = sqlx::query("UPDATE scope_notification_outbox SET withdrawn_at = now() WHERE payment_id = $1 AND kind = $2 AND sent_at IS NULL AND withdrawn_at IS NULL")
		.bind(payment)
		.bind(kind)
		.execute(&mut *conn)
		.await
		.map_err(repo_err)?;
	Ok(withdrawn.rows_affected())
}

fn repo_err(err: sqlx::Error) -> DomainError {
	DomainError::Repository(err.to_string())
}

/// A call that has not answered by then is treated as concierge being unavailable.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Calls concierge's scope inbox as the money plane: the bridge token speaks for namespace
/// `bank` there, the same trust relationship the lifecycle bridge and the approvals use.
pub struct ConciergeScopeNotifier {
	channel: Channel,
	service_token: String,
}

impl ConciergeScopeNotifier {
	pub fn new(channel: Channel, service_token: String) -> Self {
		Self { channel, service_token }
	}
}

#[async_trait]
impl ScopeNotifier for ConciergeScopeNotifier {
	async fn emit(&self, concierge_user_id: Uuid, notice: &ScopeNotice) -> Result<(), NoticeError> {
		let mut request = Request::new(EmitRequest {
			user_id: concierge_user_id.to_string(),
			topic: notice.topic.clone(),
			kind: notice.kind.clone(),
			title: notice.title.clone(),
			body: notice.body.clone(),
			link: notice.link.clone(),
			dedupe_key: notice.dedupe_key.clone(),
			occurred_at: 0,
			link_scope: String::new(),
		});
		let token: MetadataValue<_> = format!("Bearer {}", self.service_token)
			.parse()
			.map_err(|_| NoticeError::Failed("the bridge token is not a valid header value".into()))?;
		request.metadata_mut().insert("authorization", token);
		request.set_timeout(CALL_TIMEOUT);
		match ScopeInboxServiceClient::new(self.channel.clone()).emit_scope_notification(request).await {
			// `delivered: false` is a user who switched the topic off: the call succeeded.
			Ok(_) => Ok(()),
			Err(status) => Err(classify(&status)),
		}
	}
}

/// What a refusal means for the notice. INVALID_ARGUMENT "unknown topic" is concierge not
/// holding banking's catalog yet — published by the approvals executor, possibly a moment
/// behind this worker — so it waits instead of burning attempts; any other INVALID_ARGUMENT
/// is the notice itself. UNAUTHENTICATED and PERMISSION_DENIED are configuration: charged, so
/// a wrong token becomes an alert rather than a silent queue.
fn classify(status: &tonic::Status) -> NoticeError {
	let message = format!("{:?}: {}", status.code(), status.message());
	match status.code() {
		Code::InvalidArgument if status.message().contains("unknown topic") => NoticeError::Deferred(message),
		Code::Unavailable | Code::DeadlineExceeded | Code::ResourceExhausted | Code::Aborted | Code::Cancelled | Code::Unknown | Code::Internal => NoticeError::Deferred(message),
		Code::Ok
		| Code::InvalidArgument
		| Code::NotFound
		| Code::AlreadyExists
		| Code::PermissionDenied
		| Code::FailedPrecondition
		| Code::OutOfRange
		| Code::Unimplemented
		| Code::DataLoss
		| Code::Unauthenticated => NoticeError::Failed(message),
	}
}

const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const BATCH: i64 = 100;

/// After this many charged failures a row stops being retried and starts being an alert. It
/// is never deleted: the audit trail keeps what could not be delivered, and why.
pub(crate) const MAX_ATTEMPTS: i32 = 10;

/// How long a notice keeps being DEFERRED before it is given up on — a day covers a concierge
/// outage or a catalog that is slow to publish.
pub const DEFERRAL_CEILING: Duration = Duration::from_secs(24 * 60 * 60);

const MAX_DEFERRAL_BACKOFF: Duration = Duration::from_secs(60 * 60);

pub struct ScopeNotificationWorker {
	pool: PgPool,
	notifier: Arc<dyn ScopeNotifier>,
}

impl ScopeNotificationWorker {
	pub fn new(pool: PgPool, notifier: Arc<dyn ScopeNotifier>) -> Self {
		Self { pool, notifier }
	}

	/// Run as the SINGLETON worker. Two would not double-send (the emit is idempotent) but
	/// would double the attempts charged against a struggling concierge. A transient DB
	/// failure re-acquires with backoff and never returns: an early return would cancel every
	/// sibling task in the composition root's `join!`.
	pub async fn run(self, shutdown: CancellationToken) {
		const MAX_BACKOFF: Duration = Duration::from_secs(30);
		let mut backoff = Duration::from_millis(500);
		info!("scope notifications: delivering every {SWEEP_INTERVAL:?}");
		'acquire: loop {
			let mut lock = tokio::select! {
				biased;
				() = shutdown.cancelled() => return,
				lock = super::singleton::acquire(&self.pool, super::singleton::keys::SCOPE_NOTIFICATIONS) => match lock {
					Ok(lock) => lock,
					Err(err) => {
						error!("scope notifications: could not acquire the worker lock (retrying in {backoff:?}): {err}");
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
			loop {
				if let Err(err) = self.drain().await {
					warn!("scope notifications: drain failed (will retry): {err}");
				}
				tokio::select! {
					() = shutdown.cancelled() => return,
					() = tokio::time::sleep(SWEEP_INTERVAL) => {},
				}
				if !super::singleton::still_held(&mut lock).await {
					error!("scope notifications: lost the worker lock connection — re-acquiring");
					let _ = lock.close().await;
					continue 'acquire;
				}
			}
		}
	}

	/// One pass; returns how many notices were handed over. Public so a test can drive it.
	pub async fn drain(&self) -> Result<usize, DomainError> {
		let ids: Vec<i64> = sqlx::query_scalar(
			"SELECT id FROM scope_notification_outbox \
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

	/// One row in one transaction that holds its lock from the read to the outcome, across the
	/// emit: a withdrawal of the notice takes the same lock, so it either runs first (and the
	/// row is skipped here) or waits for the outcome — a notice is never handed over after the
	/// order it concerns took it back.
	async fn deliver(&self, id: i64) -> Result<bool, DomainError> {
		let mut tx = self.pool.begin().await.map_err(repo_err)?;
		let row = sqlx::query(
			"SELECT n.user_id, n.topic, n.kind, n.title, n.body, n.link, n.dedupe_key, u.concierge_user_id \
			 FROM scope_notification_outbox n JOIN users u ON u.id = n.user_id \
			 WHERE n.id = $1 AND n.sent_at IS NULL AND n.withdrawn_at IS NULL AND n.attempts < $2 AND (n.next_attempt_at IS NULL OR n.next_attempt_at <= now()) \
			 FOR UPDATE OF n SKIP LOCKED",
		)
		.bind(id)
		.bind(MAX_ATTEMPTS)
		.fetch_optional(&mut *tx)
		.await
		.map_err(repo_err)?;
		let Some(row) = row else {
			return Ok(false);
		};
		let concierge_user_id: Option<Uuid> = row.try_get("concierge_user_id").map_err(repo_err)?;
		// A person banking has not yet mirrored cannot be addressed, and may be tomorrow: their
		// first login lands the mirror. Nothing about the notice was refused, so it waits.
		let Some(recipient) = concierge_user_id else {
			self.defer(&mut tx, id, "recipient has no mirrored concierge user id").await?;
			tx.commit().await.map_err(repo_err)?;
			return Ok(false);
		};
		let notice = ScopeNotice {
			user: domain::users::UserId::from_raw(row.try_get("user_id").map_err(repo_err)?),
			topic: row.try_get("topic").map_err(repo_err)?,
			kind: row.try_get("kind").map_err(repo_err)?,
			title: row.try_get("title").map_err(repo_err)?,
			body: row.try_get("body").map_err(repo_err)?,
			link: row.try_get("link").map_err(repo_err)?,
			dedupe_key: row.try_get("dedupe_key").map_err(repo_err)?,
		};
		let sent = match self.notifier.emit(recipient, &notice).await {
			Ok(()) => {
				sqlx::query("UPDATE scope_notification_outbox SET sent_at = now() WHERE id = $1")
					.bind(id)
					.execute(&mut *tx)
					.await
					.map_err(repo_err)?;
				true
			}
			Err(NoticeError::Deferred(why)) => {
				self.defer(&mut tx, id, &why).await?;
				false
			}
			Err(NoticeError::Failed(why)) => {
				self.fail(&mut tx, id, &why).await?;
				false
			}
		};
		tx.commit().await.map_err(repo_err)?;
		Ok(sent)
	}

	async fn defer(&self, conn: &mut PgConnection, id: i64, reason: &str) -> Result<(), DomainError> {
		let expired: bool = sqlx::query_scalar("SELECT created_at + $2 < now() FROM scope_notification_outbox WHERE id = $1")
			.bind(id)
			.bind(DEFERRAL_CEILING)
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		if expired {
			self.retire(conn, id, reason).await?;
			error!(
				notice_id = id,
				"scope notifications: giving up on a notice deferred for over {DEFERRAL_CEILING:?} — its recipient will not be told: {reason}"
			);
			return Ok(());
		}
		let deferrals: i32 = sqlx::query_scalar("UPDATE scope_notification_outbox SET deferrals = deferrals + 1, last_error = $2 WHERE id = $1 RETURNING deferrals")
			.bind(id)
			.bind(reason)
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		let backoff = SWEEP_INTERVAL.saturating_mul(1u32 << (deferrals - 1).clamp(0, 16)).min(MAX_DEFERRAL_BACKOFF);
		sqlx::query("UPDATE scope_notification_outbox SET next_attempt_at = now() + $2 WHERE id = $1")
			.bind(id)
			.bind(backoff)
			.execute(&mut *conn)
			.await
			.map_err(repo_err)?;
		warn!(notice_id = id, deferrals, ?backoff, "scope notifications: delivery deferred (no attempt charged): {reason}");
		Ok(())
	}

	async fn fail(&self, conn: &mut PgConnection, id: i64, reason: &str) -> Result<(), DomainError> {
		let attempts: i32 = sqlx::query_scalar("UPDATE scope_notification_outbox SET attempts = attempts + 1, last_error = $2 WHERE id = $1 RETURNING attempts")
			.bind(id)
			.bind(reason)
			.fetch_one(&mut *conn)
			.await
			.map_err(repo_err)?;
		if attempts >= MAX_ATTEMPTS {
			error!(notice_id = id, attempts, "scope notifications: giving up on a notice — its recipient will not be told: {reason}");
		} else {
			warn!(notice_id = id, attempts, "scope notifications: delivery failed (will retry): {reason}");
		}
		Ok(())
	}

	/// Pinned at the attempt ceiling so no pass picks it up again; the row stays for the audit.
	async fn retire(&self, conn: &mut PgConnection, id: i64, reason: &str) -> Result<(), DomainError> {
		sqlx::query("UPDATE scope_notification_outbox SET attempts = $2, last_error = $3 WHERE id = $1")
			.bind(id)
			.bind(MAX_ATTEMPTS)
			.bind(reason)
			.execute(&mut *conn)
			.await
			.map_err(repo_err)?;
		Ok(())
	}
}

/// How many notices are still owed — for the boot line and for tests.
pub async fn pending_count(pool: &PgPool) -> Result<i64, DomainError> {
	sqlx::query_scalar("SELECT COUNT(*) FROM scope_notification_outbox WHERE sent_at IS NULL AND withdrawn_at IS NULL")
		.fetch_one(pool)
		.await
		.map_err(repo_err)
}
