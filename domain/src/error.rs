use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
	#[error("{entity} not found: {id}")]
	NotFound { entity: &'static str, id: String },
	#[error("conflict: {0}")]
	Conflict(String),
	#[error("validation failed: {0}")]
	Validation(String),
	/// A domain policy forbids the action for this caller in principle (e.g. an unverified
	/// caller asking for a deposit address): retrying
	/// later changes nothing. Distinct from `Validation` (bad input) and from
	/// `Precondition` (a caller who IS entitled, refused by the current state) — it maps
	/// to gRPC `permission_denied`, not `invalid_argument` or `failed_precondition`.
	#[error("forbidden: {0}")]
	Forbidden(String),
	/// The request is well-formed and the caller is entitled to make it, but the system or
	/// the aggregate is not in a state that admits it *right now* — the outflow pause, a
	/// frozen account, an investor subscribing to a product they may only view. Only the
	/// state stands in the way, and the same request goes through once it changes. Distinct
	/// from `Validation` (the input is wrong) and `Forbidden` (the caller may never): it
	/// maps to gRPC `failed_precondition`, so a client can tell "fix your request" from
	/// "ask an operator" from "this is not for you".
	#[error("precondition failed: {0}")]
	Precondition(String),
	/// Unexpected failure from a driven adapter (e.g. the database). Carries a
	/// description for logging only — it is never surfaced verbatim to clients,
	/// and an infrastructure failure must never be mapped to `Validation`.
	#[error("repository error: {0}")]
	Repository(String),
}
