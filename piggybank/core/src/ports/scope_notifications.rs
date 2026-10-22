//! The seam over which the money plane tells a person about their own money: concierge's
//! scope inbox (`ScopeInboxService.EmitScopeNotification`), on banking's namespace `bank`.
//!
//! A notice is plain text and a relative cabinet path, never markup: concierge renders the
//! inbox entry and the mail copy itself. The notice may name the scope whose cabinet the path
//! opens in; concierge, not this plane, owns the host of every cabinet. It is written to an outbox in the transaction of the
//! fact it announces and delivered by a worker, so this port only has to say how one emit
//! went.

use async_trait::async_trait;
use domain::users::UserId;

/// One notice for one person.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeNotice {
	/// The recipient's BANKING id. The worker addresses them by their identity-plane id, read
	/// from the mirror at send time, so the money plane cannot redirect a notice.
	pub user: UserId,
	/// A topic of banking's published catalog (`bank:money-movement`).
	pub topic: String,
	/// Machine-readable kind within the topic, at most 64 characters.
	pub kind: String,
	/// At most 200 characters.
	pub title: String,
	/// At most 2000 characters.
	pub body: String,
	/// Cabinet-relative path, a single leading `/`, at most 512 characters; may be empty.
	pub link: String,
	/// The scope whose cabinet `link` opens in (`[a-z][a-z0-9_]{0,31}`), or empty for the
	/// platform cabinet (`CABINET_URL`). Concierge stores the link under that scope's base when
	/// it configures one (`NOTICE_LINK_BASES`) and keeps it relative otherwise, so naming a
	/// scope with no base is harmless.
	pub link_scope: String,
	/// Idempotency key, unique per recipient, at most 128 characters.
	pub dedupe_key: String,
}

/// Why a notice was not taken, split by what the worker should do about it.
#[derive(Debug, thiserror::Error)]
pub enum NoticeError {
	/// Concierge could not be reached, throttled the call, or does not hold the topic yet (the
	/// catalog is published by another worker): nothing is wrong with the notice. Retried
	/// later without charging an attempt, until the deferral ceiling.
	#[error("deferred: {0}")]
	Deferred(String),
	/// The notice itself was refused, or the credential was: charged an attempt.
	#[error("failed: {0}")]
	Failed(String),
}

#[async_trait]
pub trait ScopeNotifier: Send + Sync {
	/// Emit one notice to the person with the identity-plane id `concierge_user_id`.
	async fn emit(&self, concierge_user_id: uuid::Uuid, notice: &ScopeNotice) -> Result<(), NoticeError>;
}
