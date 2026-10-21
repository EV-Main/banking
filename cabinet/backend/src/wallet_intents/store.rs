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
/// only one caller wins it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentStatus {
	Created,
	Authorized,
	Executed,
	Expired,
	Cancelled,
}

impl IntentStatus {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Created => "created",
			Self::Authorized => "authorized",
			Self::Executed => "executed",
			Self::Expired => "expired",
			Self::Cancelled => "cancelled",
		}
	}

	pub fn parse(raw: &str) -> Option<Self> {
		match raw {
			"created" => Some(Self::Created),
			"authorized" => Some(Self::Authorized),
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
}

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "wallet store: {}", self.0)
	}
}

/// The rolling window of the move budget.
pub const MOVE_WINDOW_SECS: u64 = 86_400;

#[async_trait]
pub trait WalletStore: Send + Sync {
	/// Store a new intent as `created` for `ttl_secs`.
	async fn put_intent(&self, intent_id: &str, record: &[u8], seal: &str, ttl_secs: u64) -> Result<(), StoreError>;

	async fn get_intent(&self, intent_id: &str) -> Result<Option<StoredIntent>, StoreError>;

	/// Compare-and-set the status: `true` only for the one caller that found `from`.
	async fn transition(&self, intent_id: &str, from: IntentStatus, to: IntentStatus) -> Result<bool, StoreError>;

	/// Spend a step-up proof's `jti`: `true` the first time, `false` for every replay
	/// until `ttl_secs` pass (concierge doc item 4: `SET stepup:jti:<jti> 1 NX EX`).
	async fn spend_jti(&self, jti: &str, ttl_secs: u64) -> Result<bool, StoreError>;

	/// Micro-USDT moved under `budget` since `since` (unix seconds).
	async fn moved_since(&self, budget: &str, since: u64) -> Result<u64, StoreError>;

	/// Count `micro` against `budget` at `now` under `entry` (unique per intent). With a
	/// `cap`, the count is taken only if the window's total stays within it, and the answer
	/// says whether it was; check and count are one atomic step.
	async fn count_move(&self, budget: &str, entry: &str, micro: u64, cap: Option<u64>, now: u64) -> Result<bool, StoreError>;
}

fn intent_key(intent_id: &str) -> String {
	format!("cabinet:wallet:intent:{intent_id}")
}

fn jti_key(jti: &str) -> String {
	format!("stepup:jti:{jti}")
}

/// The move budget of one user through one host.
pub fn move_budget(sub: &str, origin: &str) -> String {
	format!("cabinet:wallet:moved:{sub}:{origin}")
}

#[derive(Clone)]
pub struct RedisStore {
	conn: ConnectionManager,
}

impl RedisStore {
	pub async fn connect(url: &str) -> Result<Self, StoreError> {
		let client = redis::Client::open(url).map_err(|e| StoreError(format!("REDIS_URL: {e}")))?;
		let conn = client.get_connection_manager().await.map_err(|e| StoreError(format!("connect to redis: {e}")))?;
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

/// KEYS[1] = budget zset (member `<entry>:<micro>`, score = unix seconds). ARGV = now,
/// window start, entry, micro, cap ('' = none). 1 when counted.
static COUNT_MOVE: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
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
		let (record, seal, status): (Option<Vec<u8>>, Option<String>, Option<String>) = redis::cmd("HMGET")
			.arg(intent_key(intent_id))
			.arg("record")
			.arg("seal")
			.arg("status")
			.query_async(&mut conn)
			.await
			.map_err(redis_err)?;
		let (Some(record), Some(seal), Some(status)) = (record, seal, status) else {
			return Ok(None);
		};
		let status = IntentStatus::parse(&status).ok_or_else(|| StoreError(format!("intent {intent_id} has an unknown status {status:?}")))?;
		Ok(Some(StoredIntent { record, seal, status }))
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

	async fn moved_since(&self, budget: &str, since: u64) -> Result<u64, StoreError> {
		let mut conn = self.conn.clone();
		let members: Vec<String> = redis::cmd("ZRANGEBYSCORE").arg(budget).arg(since).arg("+inf").query_async(&mut conn).await.map_err(redis_err)?;
		members.iter().map(|member| entry_micro(member)).try_fold(0u64, |total, micro| Ok(total.saturating_add(micro?)))
	}

	async fn count_move(&self, budget: &str, entry: &str, micro: u64, cap: Option<u64>, now: u64) -> Result<bool, StoreError> {
		let mut conn = self.conn.clone();
		let counted: i64 = COUNT_MOVE
			.key(budget)
			.arg(now)
			.arg(now.saturating_sub(MOVE_WINDOW_SECS))
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

	async fn moved_since(&self, budget: &str, since: u64) -> Result<u64, StoreError> {
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

	async fn count_move(&self, budget: &str, _entry: &str, micro: u64, cap: Option<u64>, now: u64) -> Result<bool, StoreError> {
		let mut memory = self.lock();
		let since = now.saturating_sub(MOVE_WINDOW_SECS);
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

		let jti = format!("{tag}-jti");
		assert!(store.spend_jti(&jti, 60).await.unwrap());
		assert!(!store.spend_jti(&jti, 60).await.unwrap(), "a jti is spent once");

		let budget = format!("{tag}-budget");
		let now = 1_790_000_000;
		assert!(store.count_move(&budget, "a", 200, Some(300), now - MOVE_WINDOW_SECS - 1).await.unwrap());
		assert_eq!(store.moved_since(&budget, now - MOVE_WINDOW_SECS).await.unwrap(), 0, "outside the window");
		assert!(store.count_move(&budget, "b", 200, Some(300), now).await.unwrap(), "the old entry fell out of the window");
		assert!(!store.count_move(&budget, "c", 101, Some(300), now).await.unwrap(), "over the cap");
		assert!(store.count_move(&budget, "c", 100, Some(300), now).await.unwrap(), "exactly at the cap");
		assert!(store.count_move(&budget, "d", 1_000, None, now).await.unwrap(), "no cap: always counted");
		assert_eq!(store.moved_since(&budget, now - MOVE_WINDOW_SECS).await.unwrap(), 1_300);
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
