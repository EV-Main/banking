//! Where intents, spent proof ids and the move budget live: one store every BFF replica
//! shares. Redis in production; an in-process map for the tests and for a dev box without
//! Redis — never for more than one replica, where a proof could be spent once per process.
//!
//! The store keeps opaque bytes and a status. It knows nothing of what an intent means, and
//! it holds no key: the seal over the record is checked by the caller on every read.

use std::{
	collections::HashMap,
	sync::Mutex,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use redis::aio::ConnectionManager;

/// The life of an intent. `created → authorized` is the one transition a proof buys, and
/// only one caller wins it. `executing` is the act sent to the hub with the intent id as its
/// idempotency key: from there on a confirm may only send the same act again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentStatus {
	Created,
	Authorized,
	Executing,
	Executed,
	Expired,
	Cancelled,
}

impl IntentStatus {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Created => "created",
			Self::Authorized => "authorized",
			Self::Executing => "executing",
			Self::Executed => "executed",
			Self::Expired => "expired",
			Self::Cancelled => "cancelled",
		}
	}

	pub fn parse(raw: &str) -> Option<Self> {
		match raw {
			"created" => Some(Self::Created),
			"authorized" => Some(Self::Authorized),
			"executing" => Some(Self::Executing),
			"executed" => Some(Self::Executed),
			"expired" => Some(Self::Expired),
			"cancelled" => Some(Self::Cancelled),
			_ => None,
		}
	}
}

pub struct StoredIntent {
	pub record: Vec<u8>,
	pub seal: String,
	pub status: IntentStatus,
	/// The step-up method the act was authorised with, set on `authorized → executing` so a
	/// re-sent act carries the same audit.
	pub stepup_method: Option<String>,
}

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "wallet store: {}", self.0)
	}
}

/// The rolling window of the per-user budgets (moves without a step-up, emailed-code
/// confirmations).
pub const BUDGET_WINDOW_SECS: u64 = 86_400;

#[async_trait]
pub trait WalletStore: Send + Sync {
	/// Store a new intent as `created` for `ttl_secs`.
	async fn put_intent(&self, intent_id: &str, record: &[u8], seal: &str, ttl_secs: u64) -> Result<(), StoreError>;

	async fn get_intent(&self, intent_id: &str) -> Result<Option<StoredIntent>, StoreError>;

	/// Compare-and-set the status: `true` only for the one caller that found `from`.
	async fn transition(&self, intent_id: &str, from: IntentStatus, to: IntentStatus) -> Result<bool, StoreError>;

	/// `authorized → executing`, one winner, recording `stepup_method` and keeping the intent
	/// for `keep_secs` from now: the window in which a confirm may send the act again.
	async fn begin_execution(&self, intent_id: &str, stepup_method: &str, keep_secs: u64) -> Result<bool, StoreError>;

	/// Spend a step-up proof's `jti`: `true` the first time, `false` for every replay
	/// until `ttl_secs` pass (concierge doc item 4: `SET stepup:jti:<jti> 1 NX EX`).
	async fn spend_jti(&self, jti: &str, ttl_secs: u64) -> Result<bool, StoreError>;

	/// Micro-USDT counted under `budget` since `since` (unix seconds).
	async fn used_since(&self, budget: &str, since: u64) -> Result<u64, StoreError>;

	/// Count `micro` against `budget` at `now` under `entry` (unique per intent). With a
	/// `cap`, the count is taken only if the window's total stays within it, and the answer
	/// says whether it was; check and count are one atomic step.
	async fn count_against(&self, budget: &str, entry: &str, micro: u64, cap: Option<u64>, now: u64) -> Result<bool, StoreError>;
}

fn intent_key(intent_id: &str) -> String {
	format!("cabinet:wallet:intent:{intent_id}")
}

fn jti_key(jti: &str) -> String {
	format!("stepup:jti:{jti}")
}

/// What one user moved in 24 h, through every host together: the owners' line is a total
/// per person, so a second host must not double it.
pub fn move_budget(sub: &str) -> String {
	format!("cabinet:wallet:moved:{sub}")
}

/// What one user confirmed by emailed code in 24 h, through every host together.
pub fn email_budget(sub: &str) -> String {
	format!("cabinet:wallet:email:{sub}")
}

#[derive(Clone)]
pub struct RedisStore {
	conn: ConnectionManager,
}

impl RedisStore {
	/// Only the error's kind is reported: the URL carries the Redis password, and a parse or
	/// connect error may quote it.
	pub async fn connect(url: &str) -> Result<Self, StoreError> {
		let client = redis::Client::open(url).map_err(|e| StoreError(format!("REDIS_URL does not parse ({:?})", e.kind())))?;
		let conn = client
			.get_connection_manager()
			.await
			.map_err(|e| StoreError(format!("could not connect to REDIS_URL ({:?})", e.kind())))?;
		Ok(Self { conn })
	}
}

fn redis_err(e: redis::RedisError) -> StoreError {
	StoreError(e.to_string())
}

/// KEYS[1] = intent hash; ARGV = from, to. 1 when the status was `from` and is now `to`.
static TRANSITION: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
	redis::Script::new(
		r#"
if redis.call('HGET', KEYS[1], 'status') == ARGV[1] then
  redis.call('HSET', KEYS[1], 'status', ARGV[2])
  return 1
end
return 0
"#,
	)
});

/// KEYS[1] = intent hash; ARGV = step-up method, seconds to keep. 1 when the status was
/// `authorized` and is now `executing`.
static BEGIN_EXECUTION: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
	redis::Script::new(
		r#"
if redis.call('HGET', KEYS[1], 'status') == 'authorized' then
  redis.call('HSET', KEYS[1], 'status', 'executing', 'method', ARGV[1])
  redis.call('EXPIRE', KEYS[1], ARGV[2])
  return 1
end
return 0
"#,
	)
});

/// KEYS[1] = budget zset (member `<entry>:<micro>`, score = unix seconds). ARGV = now,
/// window start, entry, micro, cap ('' = none). 1 when counted.
static COUNT_AGAINST: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
	redis::Script::new(
		r#"
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', '(' .. ARGV[2])
if ARGV[5] ~= '' then
  local total = 0
  for _, member in ipairs(redis.call('ZRANGE', KEYS[1], 0, -1)) do
    total = total + tonumber(string.match(member, ':(%d+)$'))
  end
  if total + tonumber(ARGV[4]) > tonumber(ARGV[5]) then
    return 0
  end
end
redis.call('ZADD', KEYS[1], ARGV[1], ARGV[3] .. ':' .. ARGV[4])
redis.call('EXPIRE', KEYS[1], 86400)
return 1
"#,
	)
});

#[async_trait]
impl WalletStore for RedisStore {
	async fn put_intent(&self, intent_id: &str, record: &[u8], seal: &str, ttl_secs: u64) -> Result<(), StoreError> {
		let key = intent_key(intent_id);
		let mut conn = self.conn.clone();
		redis::pipe()
			.atomic()
			.hset_multiple(&key, &[("record", record), ("seal", seal.as_bytes()), ("status", IntentStatus::Created.as_str().as_bytes())])
			.ignore()
			.expire(&key, ttl_secs as i64)
			.ignore()
			.query_async::<()>(&mut conn)
			.await
			.map_err(redis_err)
	}

	async fn get_intent(&self, intent_id: &str) -> Result<Option<StoredIntent>, StoreError> {
		let mut conn = self.conn.clone();
		let (record, seal, status, stepup_method): (Option<Vec<u8>>, Option<String>, Option<String>, Option<String>) = redis::cmd("HMGET")
			.arg(intent_key(intent_id))
			.arg("record")
			.arg("seal")
			.arg("status")
			.arg("method")
			.query_async(&mut conn)
			.await
			.map_err(redis_err)?;
		let (Some(record), Some(seal), Some(status)) = (record, seal, status) else {
			return Ok(None);
		};
		let status = IntentStatus::parse(&status).ok_or_else(|| StoreError(format!("intent {intent_id} has an unknown status {status:?}")))?;
		Ok(Some(StoredIntent {
			record,
			seal,
			status,
			stepup_method,
		}))
	}

	async fn transition(&self, intent_id: &str, from: IntentStatus, to: IntentStatus) -> Result<bool, StoreError> {
		let mut conn = self.conn.clone();
		let won: i64 = TRANSITION
			.key(intent_key(intent_id))
			.arg(from.as_str())
			.arg(to.as_str())
			.invoke_async(&mut conn)
			.await
			.map_err(redis_err)?;
		Ok(won == 1)
	}

	async fn begin_execution(&self, intent_id: &str, stepup_method: &str, keep_secs: u64) -> Result<bool, StoreError> {
		let mut conn = self.conn.clone();
		let won: i64 = BEGIN_EXECUTION
			.key(intent_key(intent_id))
			.arg(stepup_method)
			.arg(keep_secs.max(1))
			.invoke_async(&mut conn)
			.await
			.map_err(redis_err)?;
		Ok(won == 1)
	}

	async fn spend_jti(&self, jti: &str, ttl_secs: u64) -> Result<bool, StoreError> {
		let mut conn = self.conn.clone();
		let set: Option<String> = redis::cmd("SET")
			.arg(jti_key(jti))
			.arg(1)
			.arg("NX")
			.arg("EX")
			.arg(ttl_secs.max(1))
			.query_async(&mut conn)
			.await
			.map_err(redis_err)?;
		Ok(set.is_some())
	}

	async fn used_since(&self, budget: &str, since: u64) -> Result<u64, StoreError> {
		let mut conn = self.conn.clone();
		let members: Vec<String> = redis::cmd("ZRANGEBYSCORE").arg(budget).arg(since).arg("+inf").query_async(&mut conn).await.map_err(redis_err)?;
		members.iter().map(|member| entry_micro(member)).try_fold(0u64, |total, micro| Ok(total.saturating_add(micro?)))
	}

	async fn count_against(&self, budget: &str, entry: &str, micro: u64, cap: Option<u64>, now: u64) -> Result<bool, StoreError> {
		let mut conn = self.conn.clone();
		let counted: i64 = COUNT_AGAINST
			.key(budget)
			.arg(now)
			.arg(now.saturating_sub(BUDGET_WINDOW_SECS))
			.arg(entry)
			.arg(micro)
			.arg(cap.map(|cap| cap.to_string()).unwrap_or_default())
			.invoke_async(&mut conn)
			.await
			.map_err(redis_err)?;
		Ok(counted == 1)
	}
}

fn entry_micro(member: &str) -> Result<u64, StoreError> {
	member
		.rsplit_once(':')
		.and_then(|(_, micro)| micro.parse().ok())
		.ok_or_else(|| StoreError(format!("move budget entry {member:?} is malformed")))
}

/// The in-process store. One replica only; see the module docs.
#[derive(Default)]
pub struct MemoryStore {
	inner: Mutex<Memory>,
}

#[derive(Default)]
struct Memory {
	intents: HashMap<String, (Instant, StoredIntent)>,
	jtis: HashMap<String, Instant>,
	moves: HashMap<String, Vec<(u64, u64)>>,
}

impl MemoryStore {
	fn lock(&self) -> std::sync::MutexGuard<'_, Memory> {
		// A panic while holding the lock leaves plain maps behind, never a half-written
		// entry, so the data is still sound to read.
		self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

#[async_trait]
impl WalletStore for MemoryStore {
	async fn put_intent(&self, intent_id: &str, record: &[u8], seal: &str, ttl_secs: u64) -> Result<(), StoreError> {
		let stored = StoredIntent {
			record: record.to_vec(),
			seal: seal.to_owned(),
			status: IntentStatus::Created,
			stepup_method: None,
		};
		self.lock().intents.insert(intent_id.to_owned(), (Instant::now() + Duration::from_secs(ttl_secs), stored));
		Ok(())
	}

	async fn get_intent(&self, intent_id: &str) -> Result<Option<StoredIntent>, StoreError> {
		let mut memory = self.lock();
		let now = Instant::now();
		memory.intents.retain(|_, (deadline, _)| *deadline > now);
		Ok(memory.intents.get(intent_id).map(|(_, stored)| StoredIntent {
			record: stored.record.clone(),
			seal: stored.seal.clone(),
			status: stored.status,
			stepup_method: stored.stepup_method.clone(),
		}))
	}

	async fn transition(&self, intent_id: &str, from: IntentStatus, to: IntentStatus) -> Result<bool, StoreError> {
		let mut memory = self.lock();
		match memory.intents.get_mut(intent_id) {
			Some((deadline, stored)) if *deadline > Instant::now() && stored.status == from => {
				stored.status = to;
				Ok(true)
			}
			_ => Ok(false),
		}
	}

	async fn begin_execution(&self, intent_id: &str, stepup_method: &str, keep_secs: u64) -> Result<bool, StoreError> {
		let mut memory = self.lock();
		let now = Instant::now();
		match memory.intents.get_mut(intent_id) {
			Some((deadline, stored)) if *deadline > now && stored.status == IntentStatus::Authorized => {
				stored.status = IntentStatus::Executing;
				stored.stepup_method = Some(stepup_method.to_owned());
				*deadline = now + Duration::from_secs(keep_secs.max(1));
				Ok(true)
			}
			_ => Ok(false),
		}
	}

	async fn spend_jti(&self, jti: &str, ttl_secs: u64) -> Result<bool, StoreError> {
		let mut memory = self.lock();
		let now = Instant::now();
		memory.jtis.retain(|_, deadline| *deadline > now);
		if memory.jtis.contains_key(jti) {
			return Ok(false);
		}
		memory.jtis.insert(jti.to_owned(), now + Duration::from_secs(ttl_secs.max(1)));
		Ok(true)
	}

	async fn used_since(&self, budget: &str, since: u64) -> Result<u64, StoreError> {
		let memory = self.lock();
		let total = memory
			.moves
			.get(budget)
			.into_iter()
			.flatten()
			.filter(|(at, _)| *at >= since)
			.fold(0u64, |total, (_, micro)| total.saturating_add(*micro));
		Ok(total)
	}

	async fn count_against(&self, budget: &str, _entry: &str, micro: u64, cap: Option<u64>, now: u64) -> Result<bool, StoreError> {
		let mut memory = self.lock();
		let since = now.saturating_sub(BUDGET_WINDOW_SECS);
		let entries = memory.moves.entry(budget.to_owned()).or_default();
		entries.retain(|(at, _)| *at >= since);
		let total = entries.iter().fold(0u64, |total, (_, micro)| total.saturating_add(*micro));
		if cap.is_some_and(|cap| total.saturating_add(micro) > cap) {
			return Ok(false);
		}
		entries.push((now, micro));
		Ok(true)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The contract both stores keep, run against whichever is handed in.
	async fn store_contract(store: &dyn WalletStore, tag: &str) {
		let id = format!("{tag}-intent");
		assert!(store.get_intent(&id).await.unwrap().is_none());
		store.put_intent(&id, b"{\"a\":1}", "seal", 60).await.unwrap();
		let stored = store.get_intent(&id).await.unwrap().expect("stored");
		assert_eq!(
			(stored.record.as_slice(), stored.seal.as_str(), stored.status),
			(&b"{\"a\":1}"[..], "seal", IntentStatus::Created)
		);

		assert!(store.transition(&id, IntentStatus::Created, IntentStatus::Authorized).await.unwrap(), "the first caller wins");
		assert!(!store.transition(&id, IntentStatus::Created, IntentStatus::Authorized).await.unwrap(), "the second loses");
		assert!(!store.transition("unknown", IntentStatus::Created, IntentStatus::Authorized).await.unwrap());
		assert_eq!(store.get_intent(&id).await.unwrap().unwrap().status, IntentStatus::Authorized);

		assert!(store.begin_execution(&id, "passkey", 3_600).await.unwrap(), "authorized → executing");
		assert!(!store.begin_execution(&id, "email_code", 3_600).await.unwrap(), "only from authorized, once");
		let executing = store.get_intent(&id).await.unwrap().unwrap();
		assert_eq!((executing.status, executing.stepup_method.as_deref()), (IntentStatus::Executing, Some("passkey")));
		assert!(store.transition(&id, IntentStatus::Executing, IntentStatus::Executed).await.unwrap());
		let fresh = format!("{tag}-never-authorized");
		store.put_intent(&fresh, b"{}", "seal", 60).await.unwrap();
		assert!(!store.begin_execution(&fresh, "none", 3_600).await.unwrap(), "a created intent cannot start executing");

		let jti = format!("{tag}-jti");
		assert!(store.spend_jti(&jti, 60).await.unwrap());
		assert!(!store.spend_jti(&jti, 60).await.unwrap(), "a jti is spent once");

		let budget = format!("{tag}-budget");
		let now = 1_790_000_000;
		assert!(store.count_against(&budget, "a", 200, Some(300), now - BUDGET_WINDOW_SECS - 1).await.unwrap());
		assert_eq!(store.used_since(&budget, now - BUDGET_WINDOW_SECS).await.unwrap(), 0, "outside the window");
		assert!(store.count_against(&budget, "b", 200, Some(300), now).await.unwrap(), "the old entry fell out of the window");
		assert!(!store.count_against(&budget, "c", 101, Some(300), now).await.unwrap(), "over the cap");
		assert!(store.count_against(&budget, "c", 100, Some(300), now).await.unwrap(), "exactly at the cap");
		assert!(store.count_against(&budget, "d", 1_000, None, now).await.unwrap(), "no cap: always counted");
		assert_eq!(store.used_since(&budget, now - BUDGET_WINDOW_SECS).await.unwrap(), 1_300);
	}

	#[tokio::test]
	async fn the_memory_store_keeps_the_contract() {
		store_contract(&MemoryStore::default(), "memory").await;
	}

	/// The same contract against a real Redis. Not part of the default run, because the
	/// check brings up Postgres and TigerBeetle but no Redis; run it with
	/// `REDIS_URL=redis://127.0.0.1:6379/15 cargo test -p cabinet-backend -- --ignored redis_store`.
	#[tokio::test]
	#[ignore = "needs a Redis server in REDIS_URL"]
	async fn the_redis_store_keeps_the_contract() {
		let url = std::env::var("REDIS_URL").expect("REDIS_URL names the Redis to test against");
		let store = RedisStore::connect(&url).await.expect("connect");
		let tag = super::super::intent::new_intent_id().expect("a random tag");
		store_contract(&store, &tag).await;
	}
}
