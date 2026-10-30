//! Token management — refresh-token rotation with reuse detection.
//!
//! Refresh tokens are opaque `"<family>.<secret>"` handles (not JWTs); the secret
//! is server-side state. Each presentation rotates the secret; presenting an
//! already-rotated secret is treated as theft and revokes the whole family
//! (OWASP refresh-rotation reuse detection).
//!
//! **Backing store.** [`RefreshStore`] is a thin enum over two interchangeable
//! backends with identical semantics:
//!
//! * [`InProcessRefreshStore`] — a `Mutex` map. Correct and the smallest thing that
//!   works for a single-instance dev/CI hub (mirrors the wallet BFF's session store).
//! * [`RedisRefreshStore`] — the one central Redis (`REDIS_URL`), so refresh state
//!   survives restarts and is shared across replicas. A per-service Redis is never
//!   introduced — verification stays stateless; this is the *issuing* side's state.
//!
//! [`RefreshStore::from_env`] picks Redis when `REDIS_URL` is set and falls back to
//! in-process otherwise, so unconfigured local/CI runs are unaffected. The public
//! surface is the same for both arms; the rotate/reuse decision in the Redis arm runs
//! as a single Lua script so it is atomic across replicas.

use std::{collections::HashMap, sync::Mutex};

use jsonwebtoken::get_current_timestamp;
use redis::aio::ConnectionManager;
use subtle::ConstantTimeEq;

use crate::AuthError;

/// A freshly issued refresh handle and its expiry (unix seconds).
pub struct IssuedRefresh {
	pub token: String,
	pub expires_at: u64,
}
/// The lifetime policy applied to a refresh family: the sliding window reset on
/// each rotation, the immutable absolute cap, and the idle timeout (`0` = off).
#[derive(Clone, Copy)]
pub struct SessionBounds {
	pub ttl_secs: u64,
	pub max_session_secs: u64,
	pub idle_timeout_secs: u64,
}
/// The result of a successful rotation: the `token_version` snapshot the family was
/// issued under (compared against the authoritative version to honour a "revoke all")
/// and the new handle.
pub struct RotatedRefresh {
	pub token_version_snapshot: u64,
	pub refresh: IssuedRefresh,
}
/// A read-only view of one active refresh family for the "sessions & devices" surface.
pub struct SessionView {
	pub id: String,
	pub user_agent: String,
	pub ip: String,
	pub created_at: u64,
	pub last_seen: u64,
}

/// The non-mutating classification of a presented refresh handle, keyed on its SECRET
/// half (never the family-id prefix alone). It lets a caller authorize on the real
/// credential — and detect a replayed rotated-out secret — WITHOUT performing the
/// destructive rotation, so the fallible steps of a refresh (the user lookup) can
/// run before the irreversible `prev` advance.
pub enum RefreshInspect {
	/// The presented secret is the family's CURRENT secret. Carries the owner id.
	Current { user_id: String },
	/// The presented secret is a rotated-out (PREV) secret — a replay/theft signal; the
	/// caller should revoke the family (reuse detection).
	Reuse { user_id: String },
	/// No such family, or the secret matches neither current nor prev.
	Invalid,
}

/// The refresh-token family store. Construct with [`RefreshStore::from_env`]; both
/// arms share the same async surface so callers never branch on the backend.
pub enum RefreshStore {
	InProcess(InProcessRefreshStore),
	Redis(RedisRefreshStore),
}
impl RefreshStore {
	/// Back the store with Redis when `REDIS_URL` is set; otherwise keep the
	/// in-process map (no-op-until-configured, so local/CI is unaffected).
	pub async fn from_env() -> color_eyre::Result<Self> {
		match std::env::var("REDIS_URL").ok().filter(|u| !u.is_empty()) {
			Some(url) => Ok(Self::Redis(RedisRefreshStore::connect(&url).await?)),
			None => Ok(Self::InProcess(InProcessRefreshStore::new())),
		}
	}

	pub async fn issue(&self, user_id: &str, token_version: u64, bounds: SessionBounds, user_agent: String, ip: String) -> Result<IssuedRefresh, AuthError> {
		match self {
			Self::InProcess(s) => Ok(s.issue(user_id, token_version, bounds, user_agent, ip)),
			Self::Redis(s) => s.issue(user_id, token_version, bounds, user_agent, ip).await,
		}
	}

	pub async fn rotate(&self, token: &str, bounds: SessionBounds) -> Result<RotatedRefresh, AuthError> {
		match self {
			Self::InProcess(s) => s.rotate(token, bounds),
			Self::Redis(s) => s.rotate(token, bounds).await,
		}
	}

	/// Classify a presented refresh handle by its SECRET without rotating it — the
	/// credential check for the session-management RPCs and the pre-rotation gate for
	/// refresh. See [`RefreshInspect`].
	pub async fn inspect(&self, token: &str) -> Result<RefreshInspect, AuthError> {
		match self {
			Self::InProcess(s) => Ok(s.inspect(token)),
			Self::Redis(s) => s.inspect(token).await,
		}
	}

	pub async fn revoke(&self, token: &str) -> Result<(), AuthError> {
		match self {
			Self::InProcess(s) => {
				s.revoke(token);
				Ok(())
			}
			Self::Redis(s) => s.revoke(token).await,
		}
	}

	pub async fn revoke_user(&self, user_id: &str) -> Result<(), AuthError> {
		match self {
			Self::InProcess(s) => {
				s.revoke_user(user_id);
				Ok(())
			}
			Self::Redis(s) => s.revoke_user(user_id).await,
		}
	}

	pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<SessionView>, AuthError> {
		match self {
			Self::InProcess(s) => Ok(s.list_for_user(user_id)),
			Self::Redis(s) => s.list_for_user(user_id).await,
		}
	}

	pub async fn revoke_by_id(&self, user_id: &str, id: &str) -> Result<bool, AuthError> {
		match self {
			Self::InProcess(s) => Ok(s.revoke_by_id(user_id, id)),
			Self::Redis(s) => s.revoke_by_id(user_id, id).await,
		}
	}

	pub async fn family_id_of(&self, refresh_token: &str) -> Result<Option<String>, AuthError> {
		match self {
			Self::InProcess(s) => Ok(s.family_id_of(refresh_token)),
			Self::Redis(s) => s.family_id_of(refresh_token).await,
		}
	}
}

/// In-process refresh-token family table (see module docs for the production note).
#[derive(Default)]
pub struct InProcessRefreshStore {
	families: Mutex<HashMap<String, Family>>,
}
impl InProcessRefreshStore {
	pub fn new() -> Self {
		Self::default()
	}

	/// Open a new refresh family for a user and return its first handle. `bounds`
	/// fixes the immutable absolute deadline (and idle policy) for the family's life.
	pub fn issue(&self, user_id: &str, token_version: u64, bounds: SessionBounds, user_agent: String, ip: String) -> IssuedRefresh {
		let family = uuid::Uuid::new_v4().to_string();
		let secret = uuid::Uuid::new_v4().to_string();
		let now = get_current_timestamp();
		let expires_at = now + bounds.ttl_secs;
		self.families.lock().unwrap_or_else(|e| e.into_inner()).insert(
			family.clone(),
			Family {
				id: uuid::Uuid::new_v4().to_string(),
				user_id: user_id.to_owned(),
				current: secret.clone(),
				prev: None,
				token_version,
				expires_at,
				absolute_expires_at: now + bounds.max_session_secs,
				user_agent,
				ip,
				created_at: now,
				last_seen: now,
			},
		);
		IssuedRefresh {
			token: format!("{family}.{secret}"),
			expires_at,
		}
	}

	/// Rotate a presented refresh handle. Reuse of an already-rotated secret
	/// revokes the family and is reported as [`AuthError::InvalidToken`]. The
	/// immutable absolute deadline and the idle timeout are enforced BEFORE the
	/// sliding window, so a family that has outlived either bound is dropped no
	/// matter how recently it slid `expires_at` forward.
	pub fn rotate(&self, token: &str, bounds: SessionBounds) -> Result<RotatedRefresh, AuthError> {
		let (family, secret) = token.split_once('.').ok_or(AuthError::InvalidToken)?;
		let mut map = self.families.lock().unwrap_or_else(|e| e.into_inner());
		let fam = map.get_mut(family).ok_or(AuthError::InvalidToken)?;

		let now = get_current_timestamp();
		let idle_expired = bounds.idle_timeout_secs != 0 && now.saturating_sub(fam.last_seen) > bounds.idle_timeout_secs;
		if now >= fam.absolute_expires_at || idle_expired || now >= fam.expires_at {
			map.remove(family);
			return Err(AuthError::InvalidToken);
		}

		if ct_eq(&fam.current, secret) {
			let new_secret = uuid::Uuid::new_v4().to_string();
			let expires_at = now + bounds.ttl_secs;
			fam.prev = Some(std::mem::replace(&mut fam.current, new_secret.clone()));
			fam.expires_at = expires_at;
			fam.last_seen = now;
			Ok(RotatedRefresh {
				token_version_snapshot: fam.token_version,
				refresh: IssuedRefresh {
					token: format!("{family}.{new_secret}"),
					expires_at,
				},
			})
		} else if fam.prev.as_deref().is_some_and(|prev| ct_eq(prev, secret)) {
			// Reuse of a rotated-out secret — treat the family as compromised.
			map.remove(family);
			Err(AuthError::InvalidToken)
		} else {
			Err(AuthError::InvalidToken)
		}
	}

	/// Classify a presented handle by its secret (constant-time), without mutating the
	/// family. `current` ⇒ the live credential; a rotated-out `prev` ⇒ replay/theft.
	pub fn inspect(&self, token: &str) -> RefreshInspect {
		let Some((family, secret)) = token.split_once('.') else {
			return RefreshInspect::Invalid;
		};
		let map = self.families.lock().unwrap_or_else(|e| e.into_inner());
		let Some(fam) = map.get(family) else {
			return RefreshInspect::Invalid;
		};
		if ct_eq(&fam.current, secret) {
			RefreshInspect::Current { user_id: fam.user_id.clone() }
		} else if fam.prev.as_deref().is_some_and(|prev| ct_eq(prev, secret)) {
			RefreshInspect::Reuse { user_id: fam.user_id.clone() }
		} else {
			RefreshInspect::Invalid
		}
	}

	/// Revoke a single refresh family (one logout).
	pub fn revoke(&self, token: &str) {
		if let Some((family, _)) = token.split_once('.') {
			self.families.lock().unwrap_or_else(|e| e.into_inner()).remove(family);
		}
	}

	/// Revoke every refresh family for a user (logout everywhere / revoke all).
	pub fn revoke_user(&self, user_id: &str) {
		self.families.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, f| f.user_id != user_id);
	}

	/// A view of the user's active (non-expired) refresh families — one per session.
	pub fn list_for_user(&self, user_id: &str) -> Vec<SessionView> {
		let now = get_current_timestamp();
		self.families
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.values()
			.filter(|f| f.user_id == user_id && now < f.expires_at)
			.map(|f| SessionView {
				id: f.id.clone(),
				user_agent: f.user_agent.clone(),
				ip: f.ip.clone(),
				created_at: f.created_at,
				last_seen: f.last_seen,
			})
			.collect()
	}

	/// Revoke the family with this session `id`, only if it belongs to `user_id`
	/// (guards cross-user revocation). Returns whether a family was removed.
	pub fn revoke_by_id(&self, user_id: &str, id: &str) -> bool {
		let mut map = self.families.lock().unwrap_or_else(|e| e.into_inner());
		let Some(key) = map.iter().find(|(_, f)| f.id == id && f.user_id == user_id).map(|(k, _)| k.clone()) else {
			return false;
		};
		map.remove(&key).is_some()
	}

	/// The session id of the family that owns this refresh handle, if it still exists.
	pub fn family_id_of(&self, refresh_token: &str) -> Option<String> {
		let family = refresh_token.split_once('.')?.0;
		self.families.lock().unwrap_or_else(|e| e.into_inner()).get(family).map(|f| f.id.clone())
	}

	/// Backdate a family's `created_at`/`last_seen` (and its absolute deadline) by
	/// `secs`, so the lifetime-bound tests can reach a past deadline without sleeping.
	#[cfg(test)]
	fn backdate(&self, token: &str, secs: u64) {
		let family = token.split_once('.').unwrap().0;
		let mut map = self.families.lock().unwrap_or_else(|e| e.into_inner());
		let fam = map.get_mut(family).unwrap();
		fam.created_at -= secs;
		fam.last_seen -= secs;
		fam.absolute_expires_at -= secs;
	}
}

/// Redis-backed refresh-token family store — same semantics as [`InProcessRefreshStore`],
/// shared across replicas and durable across restarts.
///
/// Layout: each family is a hash `refresh:fam:<family>` (current/prev secret, owner,
/// the per-family `token_version` snapshot, the stable session id, sliding `expires_at`,
/// the immutable `absolute_expires_at`, `last_seen`, device metadata). A per-user set
/// `refresh:user:<user_id>` indexes the family ids for listing and bulk revoke. The
/// `rotate` decision (window/idle/absolute checks, reuse detection, secret swap) runs as
/// one Lua script so it is atomic — a concurrent reuse on a second replica cannot race the
/// legitimate rotation.
#[derive(Clone)]
pub struct RedisRefreshStore {
	conn: ConnectionManager,
}
impl RedisRefreshStore {
	pub async fn connect(url: &str) -> color_eyre::Result<Self> {
		let client = redis::Client::open(url)?;
		let conn = client.get_connection_manager().await?;
		Ok(Self { conn })
	}

	fn fam_key(family: &str) -> String {
		format!("refresh:fam:{family}")
	}

	fn user_key(user_id: &str) -> String {
		format!("refresh:user:{user_id}")
	}

	pub async fn issue(&self, user_id: &str, token_version: u64, bounds: SessionBounds, user_agent: String, ip: String) -> Result<IssuedRefresh, AuthError> {
		let family = uuid::Uuid::new_v4().to_string();
		let secret = uuid::Uuid::new_v4().to_string();
		let session_id = uuid::Uuid::new_v4().to_string();
		let now = get_current_timestamp();
		let expires_at = now + bounds.ttl_secs;
		let absolute_expires_at = now + bounds.max_session_secs;

		let mut conn = self.conn.clone();
		redis::pipe()
			.atomic()
			.hset_multiple(
				Self::fam_key(&family),
				&[
					("id", session_id.as_str()),
					("user_id", user_id),
					("current", secret.as_str()),
					("token_version", &token_version.to_string()),
					("expires_at", &expires_at.to_string()),
					("absolute_expires_at", &absolute_expires_at.to_string()),
					("user_agent", &user_agent),
					("ip", &ip),
					("created_at", &now.to_string()),
					("last_seen", &now.to_string()),
				],
			)
			// TTL the family at its absolute cap, so an abandoned family is reaped even
			// if it is never rotated again (mirrors the in-process `now >= absolute`).
			.expire_at(Self::fam_key(&family), absolute_expires_at as i64)
			.sadd(Self::user_key(user_id), family.as_str())
			.expire_at(Self::user_key(user_id), absolute_expires_at as i64)
			.exec_async(&mut conn)
			.await
			.map_err(redis_err)?;

		Ok(IssuedRefresh {
			token: format!("{family}.{secret}"),
			expires_at,
		})
	}

	pub async fn rotate(&self, token: &str, bounds: SessionBounds) -> Result<RotatedRefresh, AuthError> {
		let (family, secret) = token.split_once('.').ok_or(AuthError::InvalidToken)?;
		let now = get_current_timestamp();
		let new_secret = uuid::Uuid::new_v4().to_string();
		let expires_at = now + bounds.ttl_secs;

		let mut conn = self.conn.clone();
		// Returns `{"ok", user_id, token_version}` on success, `{"reuse", user_id}` to
		// signal a compromised family that must be wiped, or nil for any other rejection.
		let outcome: Vec<String> = ROTATE_SCRIPT
			.key(Self::fam_key(family))
			.arg(secret)
			.arg(now)
			.arg(bounds.idle_timeout_secs)
			.arg(&new_secret)
			.arg(expires_at)
			.invoke_async(&mut conn)
			.await
			.map_err(redis_err)?;

		match outcome.as_slice() {
			[tag, _user_id, token_version] if tag == "ok" => {
				let token_version_snapshot = token_version.parse().unwrap_or(0);
				Ok(RotatedRefresh {
					token_version_snapshot,
					refresh: IssuedRefresh {
						token: format!("{family}.{new_secret}"),
						expires_at,
					},
				})
			}
			[tag, user_id] if tag == "reuse" => {
				// Reuse of a rotated-out secret — wipe the family and drop the user index.
				self.delete_family(family, user_id).await?;
				Err(AuthError::InvalidToken)
			}
			_ => Err(AuthError::InvalidToken),
		}
	}

	pub async fn user_of(&self, token: &str) -> Result<Option<String>, AuthError> {
		let Some(family) = token.split_once('.').map(|(f, _)| f) else {
			return Ok(None);
		};
		let mut conn = self.conn.clone();
		let user_id: Option<String> = redis::cmd("HGET").arg(Self::fam_key(family)).arg("user_id").query_async(&mut conn).await.map_err(redis_err)?;
		Ok(user_id)
	}

	/// Classify a presented handle by its secret without mutating the family (the Redis
	/// twin of [`InProcessRefreshStore::inspect`]).
	pub async fn inspect(&self, token: &str) -> Result<RefreshInspect, AuthError> {
		let Some((family, secret)) = token.split_once('.') else {
			return Ok(RefreshInspect::Invalid);
		};
		let mut conn = self.conn.clone();
		let fields: HashMap<String, String> = redis::cmd("HGETALL").arg(Self::fam_key(family)).query_async(&mut conn).await.map_err(redis_err)?;
		if fields.is_empty() {
			return Ok(RefreshInspect::Invalid);
		}
		let user_id = fields.get("user_id").cloned().unwrap_or_default();
		if fields.get("current").is_some_and(|current| ct_eq(current, secret)) {
			Ok(RefreshInspect::Current { user_id })
		} else if fields.get("prev").is_some_and(|prev| ct_eq(prev, secret)) {
			Ok(RefreshInspect::Reuse { user_id })
		} else {
			Ok(RefreshInspect::Invalid)
		}
	}

	pub async fn revoke(&self, token: &str) -> Result<(), AuthError> {
		let Some(family) = token.split_once('.').map(|(f, _)| f) else {
			return Ok(());
		};
		if let Some(user_id) = self.user_of(token).await? {
			self.delete_family(family, &user_id).await?;
		}
		Ok(())
	}

	pub async fn revoke_user(&self, user_id: &str) -> Result<(), AuthError> {
		let mut conn = self.conn.clone();
		let families: Vec<String> = redis::cmd("SMEMBERS").arg(Self::user_key(user_id)).query_async(&mut conn).await.map_err(redis_err)?;
		let mut pipe = redis::pipe();
		pipe.atomic();
		for family in &families {
			pipe.del(Self::fam_key(family));
		}
		pipe.del(Self::user_key(user_id));
		pipe.exec_async(&mut conn).await.map_err(redis_err)?;
		Ok(())
	}

	pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<SessionView>, AuthError> {
		let mut conn = self.conn.clone();
		let families: Vec<String> = redis::cmd("SMEMBERS").arg(Self::user_key(user_id)).query_async(&mut conn).await.map_err(redis_err)?;
		let now = get_current_timestamp();
		let mut views = Vec::new();
		let mut stale = Vec::new();
		for family in families {
			let fields: HashMap<String, String> = redis::cmd("HGETALL").arg(Self::fam_key(&family)).query_async(&mut conn).await.map_err(redis_err)?;
			if fields.is_empty() {
				// The family hash expired (TTL) but the user-index entry lingered — prune it.
				stale.push(family);
				continue;
			}
			let expires_at = fields.get("expires_at").and_then(|v| v.parse().ok()).unwrap_or(0);
			if now >= expires_at {
				continue;
			}
			views.push(SessionView {
				id: fields.get("id").cloned().unwrap_or_default(),
				user_agent: fields.get("user_agent").cloned().unwrap_or_default(),
				ip: fields.get("ip").cloned().unwrap_or_default(),
				created_at: fields.get("created_at").and_then(|v| v.parse().ok()).unwrap_or(0),
				last_seen: fields.get("last_seen").and_then(|v| v.parse().ok()).unwrap_or(0),
			});
		}
		if !stale.is_empty() {
			let mut pipe = redis::pipe();
			pipe.atomic();
			for family in &stale {
				pipe.srem(Self::user_key(user_id), family);
			}
			pipe.exec_async(&mut conn).await.map_err(redis_err)?;
		}
		Ok(views)
	}

	pub async fn revoke_by_id(&self, user_id: &str, id: &str) -> Result<bool, AuthError> {
		let mut conn = self.conn.clone();
		let families: Vec<String> = redis::cmd("SMEMBERS").arg(Self::user_key(user_id)).query_async(&mut conn).await.map_err(redis_err)?;
		for family in families {
			let session_id: Option<String> = redis::cmd("HGET").arg(Self::fam_key(&family)).arg("id").query_async(&mut conn).await.map_err(redis_err)?;
			if session_id.as_deref() == Some(id) {
				self.delete_family(&family, user_id).await?;
				return Ok(true);
			}
		}
		Ok(false)
	}

	pub async fn family_id_of(&self, refresh_token: &str) -> Result<Option<String>, AuthError> {
		let Some(family) = refresh_token.split_once('.').map(|(f, _)| f) else {
			return Ok(None);
		};
		let mut conn = self.conn.clone();
		let id: Option<String> = redis::cmd("HGET").arg(Self::fam_key(family)).arg("id").query_async(&mut conn).await.map_err(redis_err)?;
		Ok(id)
	}

	async fn delete_family(&self, family: &str, user_id: &str) -> Result<(), AuthError> {
		let mut conn = self.conn.clone();
		redis::pipe()
			.atomic()
			.del(Self::fam_key(family))
			.srem(Self::user_key(user_id), family)
			.exec_async(&mut conn)
			.await
			.map_err(redis_err)
	}
}

/// The atomic rotate/reuse decision. KEYS[1] = family hash. ARGV = presented secret,
/// now, idle_timeout_secs, new secret, new sliding expiry. Returns `{"ok", user_id,
/// token_version}` on a successful rotation, `{"reuse", user_id}` when an already-rotated
/// secret is replayed (caller wipes the family), or nil for any other rejection
/// (missing/expired/wrong secret).
static ROTATE_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
	redis::Script::new(
		r#"
local fam = redis.call('HGETALL', KEYS[1])
if #fam == 0 then return nil end
local h = {}
for i = 1, #fam, 2 do h[fam[i]] = fam[i + 1] end

local now = tonumber(ARGV[2])
local idle = tonumber(ARGV[3])
local absolute = tonumber(h['absolute_expires_at'])
local expires = tonumber(h['expires_at'])
local last_seen = tonumber(h['last_seen'])

if now >= absolute or now >= expires or (idle ~= 0 and (now - last_seen) > idle) then
  redis.call('DEL', KEYS[1])
  return nil
end

if h['current'] == ARGV[1] then
  redis.call('HSET', KEYS[1], 'prev', h['current'], 'current', ARGV[4], 'expires_at', ARGV[5], 'last_seen', ARGV[2])
  return { 'ok', h['user_id'], h['token_version'] }
elseif h['prev'] == ARGV[1] then
  return { 'reuse', h['user_id'] }
else
  return nil
end
"#,
	)
});

fn redis_err(e: redis::RedisError) -> AuthError {
	crate::telemetry::report(&e);
	AuthError::Unavailable
}

/// Constant-time string equality for refresh secrets: an early length check (which
/// only reveals length, never a guessed value) then a byte-wise `ConstantTimeEq` that
/// does not short-circuit on the first differing byte.
fn ct_eq(a: &str, b: &str) -> bool {
	a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

struct Family {
	/// Stable session id, preserved across rotations (the token handle changes,
	/// this does not), so the "sessions & devices" surface can address a session.
	id: String,
	user_id: String,
	current: String,
	prev: Option<String>,
	/// The user's `token_version` at issue time, so a later "revoke all" (which
	/// bumps the authoritative version in Postgres) is detected on the next refresh.
	token_version: u64,
	/// Sliding expiry, reset to `now + ttl_secs` on every rotation.
	expires_at: u64,
	/// Immutable absolute deadline stamped at issue time (`created_at + max_session_secs`);
	/// rotation past it is refused regardless of the sliding `expires_at`.
	absolute_expires_at: u64,
	user_agent: String,
	ip: String,
	created_at: u64,
	last_seen: u64,
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A wide absolute cap and no idle timeout: the default for tests that exercise
	/// only the sliding-window / reuse behaviour.
	fn bounds() -> SessionBounds {
		SessionBounds {
			ttl_secs: 3600,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 0,
		}
	}

	fn issue(store: &InProcessRefreshStore, user_id: &str) -> IssuedRefresh {
		store.issue(user_id, 0, bounds(), String::new(), String::new())
	}

	#[test]
	fn rotate_then_reuse_revokes_family() {
		let store = InProcessRefreshStore::new();
		let issued = issue(&store, "user-1");
		let rotated = store.rotate(&issued.token, bounds()).unwrap();
		assert!(matches!(store.inspect(&rotated.refresh.token), RefreshInspect::Current { user_id } if user_id == "user-1"));
		// The original (now rotated-out) secret is a reuse → family revoked.
		assert!(store.rotate(&issued.token, bounds()).is_err());
		// And the just-issued one is now dead too.
		assert!(store.rotate(&rotated.refresh.token, bounds()).is_err());
	}

	#[test]
	fn revoke_user_drops_all_families() {
		let store = InProcessRefreshStore::new();
		let a = issue(&store, "user-1");
		let b = issue(&store, "user-1");
		store.revoke_user("user-1");
		assert!(store.rotate(&a.token, bounds()).is_err());
		assert!(store.rotate(&b.token, bounds()).is_err());
	}

	#[test]
	fn inspect_authorizes_only_the_current_secret() {
		let store = InProcessRefreshStore::new();
		let issued = store.issue("user-1", 5, bounds(), String::new(), String::new());

		// The live token authorizes as its family's user.
		match store.inspect(&issued.token) {
			RefreshInspect::Current { user_id } => assert_eq!(user_id, "user-1"),
			_ => panic!("a live token must inspect as Current"),
		}
		// A right-family / wrong-secret handle (the family-id prefix alone) is NOT a
		// credential — this is the session-RPC hardening.
		let (family, _) = issued.token.split_once('.').unwrap();
		assert!(matches!(store.inspect(&format!("{family}.not-the-secret")), RefreshInspect::Invalid));
		assert!(matches!(store.inspect("no-dot"), RefreshInspect::Invalid));

		// After a rotation the prior secret inspects as Reuse (theft signal) WITHOUT being
		// mutated by the inspection, and the new secret is Current.
		let rotated = store.rotate(&issued.token, bounds()).unwrap();
		assert!(matches!(store.inspect(&issued.token), RefreshInspect::Reuse { .. }));
		assert!(matches!(store.inspect(&rotated.refresh.token), RefreshInspect::Current { .. }));
		// Inspection did not revoke the family (unlike a rotate of the reused secret).
		assert!(matches!(store.inspect(&rotated.refresh.token), RefreshInspect::Current { .. }));
	}

	#[test]
	fn session_id_is_stable_across_rotation() {
		let store = InProcessRefreshStore::new();
		let issued = store.issue("user-1", 0, bounds(), "agent".into(), "1.2.3.4".into());
		let id = store.family_id_of(&issued.token).unwrap();
		let rotated = store.rotate(&issued.token, bounds()).unwrap();
		assert_eq!(store.family_id_of(&rotated.refresh.token).as_deref(), Some(id.as_str()));
		let sessions = store.list_for_user("user-1");
		assert_eq!(sessions.len(), 1);
		assert_eq!(sessions[0].id, id);
		assert_eq!(sessions[0].user_agent, "agent");
		assert!(sessions[0].last_seen >= sessions[0].created_at);
	}

	#[test]
	fn revoke_by_id_guards_cross_user() {
		let store = InProcessRefreshStore::new();
		let mine = store.issue("user-1", 0, bounds(), String::new(), String::new());
		let id = store.family_id_of(&mine.token).unwrap();
		// A different user cannot revoke it.
		assert!(!store.revoke_by_id("user-2", &id));
		assert!(store.rotate(&mine.token, bounds()).is_ok());
		// The owner can; a second attempt is a no-op.
		let id = store.family_id_of(&mine.token).unwrap();
		assert!(store.revoke_by_id("user-1", &id));
		assert!(!store.revoke_by_id("user-1", &id));
		assert!(store.list_for_user("user-1").is_empty());
	}

	#[test]
	fn rotation_succeeds_within_absolute_window() {
		// Absolute cap of one day; the family is 1h old and the sliding TTL is fresh.
		let bounds = SessionBounds {
			ttl_secs: 3600,
			max_session_secs: 86_400,
			idle_timeout_secs: 0,
		};
		let store = InProcessRefreshStore::new();
		let issued = store.issue("user-1", 0, bounds, String::new(), String::new());
		store.backdate(&issued.token, 3600);
		assert!(store.rotate(&issued.token, bounds).is_ok());
	}

	#[test]
	fn rotation_fails_past_absolute_window_despite_sliding() {
		// A long sliding TTL would keep the family alive forever; the absolute cap
		// of 86_400s must still drop it once the family is older than a day, even
		// though the sliding `expires_at` is nowhere near.
		let bounds = SessionBounds {
			ttl_secs: 2_592_000,
			max_session_secs: 86_400,
			idle_timeout_secs: 0,
		};
		let store = InProcessRefreshStore::new();
		let issued = store.issue("user-1", 0, bounds, String::new(), String::new());
		store.backdate(&issued.token, 86_401);
		assert!(store.rotate(&issued.token, bounds).is_err());
		// The expired family is removed, not merely refused.
		assert!(store.list_for_user("user-1").is_empty());
	}

	#[test]
	fn rotation_fails_past_idle_timeout() {
		// No activity for longer than the idle window revokes the family even though
		// both the sliding and absolute deadlines are far in the future.
		let bounds = SessionBounds {
			ttl_secs: 2_592_000,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 3600,
		};
		let store = InProcessRefreshStore::new();
		let issued = store.issue("user-1", 0, bounds, String::new(), String::new());
		store.backdate(&issued.token, 3601);
		assert!(store.rotate(&issued.token, bounds).is_err());
		assert!(store.list_for_user("user-1").is_empty());
	}

	#[test]
	fn rotation_resets_idle_clock() {
		// A rotation within the idle window refreshes `last_seen`, so a subsequent
		// rotation just under the window again succeeds.
		let bounds = SessionBounds {
			ttl_secs: 2_592_000,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 3600,
		};
		let store = InProcessRefreshStore::new();
		let issued = store.issue("user-1", 0, bounds, String::new(), String::new());
		store.backdate(&issued.token, 1800);
		let rotated = store.rotate(&issued.token, bounds).unwrap();
		store.backdate(&rotated.refresh.token, 1800);
		assert!(store.rotate(&rotated.refresh.token, bounds).is_ok());
	}
}

/// Real-Redis round-trips for [`RedisRefreshStore`]. No mocks — these hit the Redis at
/// `REDIS_URL` (CI brings one up) and are skipped when it is unset locally. They prove the Redis arm matches the in-process semantics:
/// rotation, reuse detection, the lifetime bounds, listing, and the revoke surfaces.
#[cfg(test)]
mod redis_tests {
	use super::*;

	fn bounds() -> SessionBounds {
		SessionBounds {
			ttl_secs: 3600,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 0,
		}
	}

	/// Connect to the configured Redis, or `None` to skip when `REDIS_URL` is unset.
	/// A unique user id per test keeps concurrent test runs from colliding on keys.
	async fn store() -> Option<(RedisRefreshStore, String)> {
		let Some(url) = std::env::var("REDIS_URL").ok().filter(|u| !u.is_empty()) else {
			assert!(std::env::var("CI").is_err(), "REDIS_URL required in CI");
			return None;
		};
		let store = RedisRefreshStore::connect(&url).await.expect("connect to REDIS_URL");
		Some((store, format!("user-{}", uuid::Uuid::new_v4())))
	}

	#[tokio::test]
	async fn rotate_then_reuse_revokes_family() {
		let Some((store, user)) = store().await else {
			return;
		};
		let issued = store.issue(&user, 7, bounds(), "agent".into(), "1.2.3.4".into()).await.unwrap();
		let rotated = store.rotate(&issued.token, bounds()).await.unwrap();
		assert_eq!(rotated.token_version_snapshot, 7);
		assert!(matches!(store.inspect(&rotated.refresh.token).await.unwrap(), RefreshInspect::Current { user_id } if user_id == user));
		// Reuse of the rotated-out secret revokes the whole family.
		assert!(store.rotate(&issued.token, bounds()).await.is_err());
		assert!(store.rotate(&rotated.refresh.token, bounds()).await.is_err());
		assert!(store.list_for_user(&user).await.unwrap().is_empty());
	}

	#[tokio::test]
	async fn revoke_user_drops_all_families() {
		let Some((store, user)) = store().await else {
			return;
		};
		let a = store.issue(&user, 0, bounds(), String::new(), String::new()).await.unwrap();
		let b = store.issue(&user, 0, bounds(), String::new(), String::new()).await.unwrap();
		store.revoke_user(&user).await.unwrap();
		assert!(store.rotate(&a.token, bounds()).await.is_err());
		assert!(store.rotate(&b.token, bounds()).await.is_err());
		assert!(store.list_for_user(&user).await.unwrap().is_empty());
	}

	#[tokio::test]
	async fn session_id_is_stable_across_rotation() {
		let Some((store, user)) = store().await else {
			return;
		};
		let issued = store.issue(&user, 0, bounds(), "agent".into(), "1.2.3.4".into()).await.unwrap();
		let id = store.family_id_of(&issued.token).await.unwrap().expect("a fresh family has a session id");
		let rotated = store.rotate(&issued.token, bounds()).await.unwrap();
		assert_eq!(store.family_id_of(&rotated.refresh.token).await.unwrap().as_deref(), Some(id.as_str()));
		let sessions = store.list_for_user(&user).await.unwrap();
		assert_eq!(sessions.len(), 1);
		assert_eq!(sessions[0].id, id);
		assert_eq!(sessions[0].user_agent, "agent");
		store.revoke_user(&user).await.unwrap();
	}

	#[tokio::test]
	async fn revoke_by_id_guards_cross_user() {
		let Some((store, user)) = store().await else {
			return;
		};
		let other = format!("other-{}", uuid::Uuid::new_v4());
		let mine = store.issue(&user, 0, bounds(), String::new(), String::new()).await.unwrap();
		let id = store.family_id_of(&mine.token).await.unwrap().unwrap();
		// A different user cannot revoke it.
		assert!(!store.revoke_by_id(&other, &id).await.unwrap());
		assert!(store.rotate(&mine.token, bounds()).await.is_ok());
		// The owner can; a second attempt is a no-op.
		let id = store.family_id_of(&mine.token).await.unwrap().unwrap();
		assert!(store.revoke_by_id(&user, &id).await.unwrap());
		assert!(!store.revoke_by_id(&user, &id).await.unwrap());
		assert!(store.list_for_user(&user).await.unwrap().is_empty());
	}

	#[tokio::test]
	async fn rotation_fails_past_idle_timeout() {
		let Some((store, user)) = store().await else {
			return;
		};
		// A 1s idle window with a now-2s-stale family: the next rotation must drop it
		// even though the sliding and absolute deadlines are far in the future.
		let bounds = SessionBounds {
			ttl_secs: 3600,
			max_session_secs: 7_776_000,
			idle_timeout_secs: 1,
		};
		let issued = store.issue(&user, 0, bounds, String::new(), String::new()).await.unwrap();
		tokio::time::sleep(std::time::Duration::from_secs(2)).await;
		assert!(store.rotate(&issued.token, bounds).await.is_err());
		assert!(store.list_for_user(&user).await.unwrap().is_empty());
	}
}
