//! Shared bring-up for the integration tests: real Postgres **and** TigerBeetle, no
//! mocks. Every suite here opens the same two connections the same way, so the steps
//! live once instead of once per file — including the one decision about a missing
//! service: skip locally, fail under CI (see [`skip_or_fail`]).
//!
//! Each test binary gets **its own database**, cloned from a migrated template (see
//! [`database_url`]). Two things forced that: the outbox relay's advisory lock is scoped to
//! a database, so binaries sharing one (two `cargo test --test …` side by side) raced for it
//! and timed out; and one shared database migrated by another branch's build failed every
//! suite with "migration N was previously applied but has been modified". The unit is the
//! binary, not the process: a runner that forks one process per *test* (nextest) would have
//! every process drop and re-create the same clone under its siblings — that needs a
//! per-process name and is not supported here.
//!
//! Each integration test is its own crate, so a suite that uses only part of this
//! module still compiles the rest — hence the blanket `dead_code` allowance.
#![allow(dead_code)]

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use domain::{
	balance::ServiceId,
	error::DomainError,
	fees::FeePolicy,
	money::{Network, WalletAddress},
	users::UserId,
};
use piggybank_core::{
	infrastructure::{
		db,
		ledger::{self, TbLedger},
		relay::Relay,
		tigerbeetle::TigerBeetle,
	},
	ports::{deposit_addresses::DepositAddresses, ledger::Ledger},
};
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool, migrate::MigrateError, postgres::PgPoolOptions};
use tokio::sync::OnceCell;

/// Serializes every test that works this binary's outbox — the one rule production gets
/// from the relay's advisory lock, restated for a process that runs several relays.
///
/// Two ways in, one lock, deliberately. A test that runs
/// [`Relay::run`](piggybank_core::infrastructure::relay::Relay::run) or takes the outbox
/// advisory lock (`acquire_outbox_lock`) needs it because the lock is one per database, so a
/// sibling doing the same blocks — a driver polling for "the relay applied my row" then times
/// out while `run` is still queued behind it. A test that only calls the unfenced
/// `Relay::drain` never touches that lock, but shares the table it drains, and two of a
/// binary's relays draining at once pick up the same row and both act on it. Two shapes have
/// been observed. The loser's `saga_steps` insert trips the table's second unique key
/// (`tb_transfer_id`), which its `ON CONFLICT (event_id, leg)` does not cover; the relay files
/// that as a transient failure and `drain()` returns early, leaving the calling test's own
/// rows queued, so it reads a balance the relay has not landed yet (#294/#298). And a row ends
/// up with `dispatched_at` *and* `parked_at` set, each written by a different custody fake —
/// the mechanism behind the `relay_recovery` flakes.
///
/// One lock for both, not one per reason: a mutex per reason serializes each class against
/// itself and neither against the other, so a `run` test and a `drain` test in one binary
/// would race over the same outbox with both guards held and nothing to show for it. Take it
/// once per test — `tokio::sync::Mutex` is not reentrant.
///
/// Scope: the outbox and its lock live in this binary's own database (see [`database_url`]),
/// so another binary — even one running at the same time — can never reach them. What remains
/// to serialize is the tests *inside* one binary, and a `LazyLock` in one process is exactly
/// that. Production has a single drainer under the advisory lock and needs none of this.
static OUTBOX: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Hold for the whole life of a test that runs `Relay::run`, takes the outbox lock, or calls
/// `Relay::drain` — not just around the call. Rows are visible to a sibling's drain from the
/// moment they commit, which is before the enqueuing test gets to its own `drain()`. Suites
/// keep the guard in their harness so no test can forget it. See [`OUTBOX`].
pub async fn outbox_serial() -> tokio::sync::MutexGuard<'static, ()> {
	OUTBOX.lock().await
}

/// Drain the outbox to quiescence. `Relay::drain` applies one pass and answers `true` when
/// a transient failure told it to back off — `Relay::run` sleeps and comes back, so a test
/// that took one pass for "everything landed" read the ledger before its own rows did
/// (#294). A few passes, each after a short pause, cover a real hiccup — a ledger that is
/// away for tens of milliseconds, not microseconds; a backlog still standing after them is a
/// finding, named by the outbox's own reasons instead of surfacing as a wrong balance later.
///
/// A park is the other way an event fails to land, and `drain` cannot report it: `next_batch`
/// skips parked rows, so the pass after a park returns `Drained` — "the outbox is empty" and
/// "the last live row was parked" are the same `false`. Reading that as "it all landed" is
/// exactly the silent wrong balance this helper exists to prevent, and it is reachable rather
/// than theoretical — the settle-time liquidity pre-check parks a whole disbursement when the
/// rail is short. So a park raised during the call is reported here, with the relay's reason.
///
/// The caller holds [`outbox_serial`]: without it a sibling's relay is draining the same
/// table, and the early return this loop retries past is exactly what that race produces.
pub async fn drain_to_quiescence(relay: &Relay, pool: &PgPool) {
	const PASSES: usize = 5;
	const BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
	// Parked is terminal and the outbox is one table per binary, so a row parked earlier in
	// the run stays there: only the rows this call parked are this caller's finding.
	let parked_before: Vec<i64> = sqlx::query_scalar("SELECT seq FROM outbox WHERE parked_at IS NOT NULL")
		.fetch_all(pool)
		.await
		.expect("read the outbox's parked rows");
	for _ in 0..PASSES {
		if !relay.drain().await {
			let parked: Vec<(i64, String, Option<String>)> = sqlx::query_as("SELECT seq, kind, last_error FROM outbox WHERE parked_at IS NOT NULL AND seq <> ALL($1) ORDER BY seq")
				.bind(parked_before.as_slice())
				.fetch_all(pool)
				.await
				.expect("read the outbox's parked rows");
			assert!(parked.is_empty(), "the relay parked an event instead of applying it: {parked:?}");
			return;
		}
		tokio::time::sleep(BACKOFF).await;
	}
	let backlog: Vec<(i64, String, Option<String>)> = sqlx::query_as("SELECT seq, kind, last_error FROM outbox WHERE dispatched_at IS NULL AND parked_at IS NULL ORDER BY seq")
		.fetch_all(pool)
		.await
		.expect("read the outbox backlog");
	assert!(
		!backlog.is_empty(),
		"drain kept throttling for {PASSES} passes with nothing left queued — the relay's own read of the outbox is what failed (see its `relay:` warnings)"
	);
	panic!("the outbox still holds a backlog after {PASSES} relay passes: {backlog:?}");
}

/// True under CI: `CI` is set, non-empty and not `"0"`/`"false"`. GitHub Actions exports
/// `CI=true`, and the flake's `check-rust` gate does the same.
fn ci() -> bool {
	std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// The one exit for "an integration service is missing". Locally that is a skip (`None`,
/// the caller prints its own notice); in CI it is a failure — a green run that executed
/// zero integration tests is exactly the outcome CI exists to catch (#259).
fn skip_or_fail<T>(reason: &str) -> Option<T> {
	if ci() {
		panic!("integration services required in CI: {reason} (or unset CI to skip locally)");
	}
	None
}

/// The URL of this test binary's own database, provisioned on first call and cached for
/// the rest of the process; `None` when `DATABASE_URL` is unset locally, a panic under CI
/// (see [`skip_or_fail`]). For suites that build their own pool (`db::connect_sized`,
/// custom options); everyone else wants [`pool`].
///
/// `DATABASE_URL` names the *base* database (`…/banking_c`). Next to it live
/// `<base>_template`, migrated once and shared by every binary, and one
/// `<base>_<binary>` per test binary, cloned from the template with `CREATE DATABASE …
/// TEMPLATE` — a file copy, far cheaper than replaying the migrations. The clone is
/// dropped and re-created at the start of each run of the binary, so a run always starts
/// from a clean schema. Nothing drops it at the end: a Rust test binary has no
/// end-of-process hook, and a leftover `<base>_<binary>` costs nothing but disk until the
/// next run replaces it.
///
/// Provisioning holds a session-level advisory lock on the base database, so binaries
/// started in parallel (two `cargo test` invocations) take turns at the template instead of
/// racing to create it. Two processes of the *same* binary at once are not isolated: they
/// share the clone's name, and the second drops it under the first.
pub async fn database_url() -> Option<String> {
	let base_url = match std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty()) {
		Some(url) => url,
		None => return skip_or_fail("DATABASE_URL unset — run `nix run .#db` and export DATABASE_URL"),
	};
	// A tokio `OnceCell` (unlike `std::sync::OnceLock`) can await inside `get_or_init`, and
	// its semaphore is runtime-agnostic — each `#[tokio::test]` in this binary runs on its
	// own runtime, and they all share this one cell.
	static URL: OnceCell<String> = OnceCell::const_new();
	Some(URL.get_or_init(|| provision_binary_database(base_url)).await.clone())
}

/// Advisory-lock key for the provisioning critical section, taken on the base database.
/// Any fixed value works as long as it is the same in every binary; this one spells
/// `pgtstdb!` so it is recognizable in `pg_locks`.
const PROVISION_LOCK_KEY: i64 = 0x7067_7473_7464_6221;

/// Postgres truncates identifiers to `NAMEDATALEN - 1` bytes silently; truncating
/// ourselves keeps the name we connect to equal to the name we created.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// Leaves `MAX_IDENTIFIER_BYTES - MAX_BASE_NAME_BYTES - 1` = 30 bytes for the binary name —
/// longer than any suite under `tests/` — so no two binaries truncate onto one clone.
const MAX_BASE_NAME_BYTES: usize = 32;

/// Provision this binary's database: lock, ensure a migrated template, clone it under this
/// binary's name, unlock. Returns the clone's URL.
async fn provision_binary_database(base_url: String) -> String {
	let base_name = database_name(&base_url);
	assert!(
		!base_name.is_empty() && base_name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
		"DATABASE_URL must name a plain lowercase database (got {base_name:?}): the tests derive `{base_name}_template` and `{base_name}_<binary>` from it and splice them into DDL unquoted"
	);
	// Postgres truncates identifiers silently, so a long base name would make `_template`
	// and the clones collide or disappear; keep room for the longest suite name.
	assert!(
		base_name.len() <= MAX_BASE_NAME_BYTES,
		"DATABASE_URL database name {base_name:?} is longer than {MAX_BASE_NAME_BYTES} bytes — `{base_name}_<binary>` would no longer fit a Postgres identifier"
	);
	let template = format!("{base_name}_template");
	let binary_db = binary_database_name(&base_name);

	let mut admin = PgConnection::connect(&base_url).await.expect("connect to Postgres (DATABASE_URL)");
	sqlx::query("SELECT pg_advisory_lock($1)")
		.bind(PROVISION_LOCK_KEY)
		.execute(&mut admin)
		.await
		.expect("take the test-database provisioning lock");

	ensure_migrated_template(&mut admin, &base_url, &template).await;

	// A clone from the last run may still exist (nothing drops it at exit); FORCE also
	// evicts a straggler connection from a binary that was killed mid-test.
	sqlx::query(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {binary_db} WITH (FORCE)")))
		.execute(&mut admin)
		.await
		.expect("drop the previous run's test database");
	sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {binary_db} TEMPLATE {template}")))
		.execute(&mut admin)
		.await
		.expect("clone the test database from the migrated template");

	sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
		.bind(PROVISION_LOCK_KEY)
		.fetch_one(&mut admin)
		.await
		.expect("release the test-database provisioning lock");
	admin.close().await.expect("close the admin connection");

	swap_db(&base_url, &binary_db)
}

/// Leave `<template>` existing and migrated to exactly this build's migration set, with no
/// connection of ours still attached (`CREATE DATABASE … TEMPLATE` refuses a template that
/// has other sessions). Caller holds the provisioning lock.
///
/// The template outlives builds, so it can carry another branch's migrations: a file that
/// was edited (`VersionMismatch`) or a version this build does not know (`VersionMissing`,
/// after switching branches) or a migration left half-applied by a killed run (`Dirty`). All
/// mean "not our schema" and are answered by rebuilding the template from scratch rather
/// than failing every suite.
async fn ensure_migrated_template(admin: &mut PgConnection, base_url: &str, template: &str) {
	let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
		.bind(template)
		.fetch_one(&mut *admin)
		.await
		.expect("look up the template database");
	if !exists {
		create_database(admin, template).await;
	}

	let template_url = swap_db(base_url, template);
	match migrate_template(&template_url).await {
		Ok(()) => {}
		Err(err @ (MigrateError::VersionMismatch(_) | MigrateError::VersionMissing(_) | MigrateError::Dirty(_))) => {
			eprintln!("test template database {template} was migrated by another build ({err}) — recreating it");
			sqlx::query(AssertSqlSafe(format!("DROP DATABASE {template} WITH (FORCE)")))
				.execute(&mut *admin)
				.await
				.expect("drop the stale template database");
			create_database(admin, template).await;
			migrate_template(&template_url).await.expect("migrate the recreated template database");
		}
		Err(err) => panic!("migrating the test template database {template}: {err}"),
	}
}

/// Migrate `template_url` with this build's embedded migrations and disconnect. The
/// `migrate!()` here resolves `./migrations` against this crate's manifest, so it is the
/// same set `db::migrate` embeds — that helper is not used because it erases the error
/// into `color_eyre::Report`, and the caller needs the typed [`MigrateError`].
async fn migrate_template(template_url: &str) -> Result<(), MigrateError> {
	let pool = PgPoolOptions::new().max_connections(1).connect(template_url).await.expect("connect to the template database");
	let result = sqlx::migrate!().run(&pool).await;
	pool.close().await;
	result
}

async fn create_database(admin: &mut PgConnection, name: &str) {
	// The identifier is derived locally from a validated base name — not user input.
	sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
		.execute(admin)
		.await
		.unwrap_or_else(|err| panic!("create database {name}: {err}"));
}

/// `<base>_<binary>`, where `<binary>` is this test binary's file name minus cargo's
/// `-<hash>` suffix, lowercased and reduced to `[a-z0-9_]`, and the whole thing cut to fit a
/// Postgres identifier.
fn binary_database_name(base_name: &str) -> String {
	let exe = std::env::current_exe().expect("locate the running test binary");
	let file_name = exe.file_name().and_then(|n| n.to_str()).expect("test binary has a UTF-8 file name");
	let stem = file_name.strip_suffix(".exe").unwrap_or(file_name);
	// Cargo names test binaries `<target>-<16 hex digits>`; a stem with no such suffix is
	// used as is.
	let stem = match stem.rsplit_once('-') {
		Some((name, hash)) if !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()) => name,
		_ => stem,
	};
	let mut binary: String = stem
		.chars()
		.map(|c| match c.to_ascii_lowercase() {
			c @ ('a'..='z' | '0'..='9' | '_') => c,
			_ => '_',
		})
		.collect();
	if binary.is_empty() {
		binary.push_str("test");
	}
	let budget = MAX_IDENTIFIER_BYTES.saturating_sub(base_name.len() + 1);
	binary.truncate(budget);
	format!("{base_name}_{binary}")
}

/// The database path segment of a `postgres://user:pass@host:port/db[?p]` URL.
fn database_name(url: &str) -> String {
	let base = url.split_once('?').map_or(url, |(base, _)| base);
	let authority_start = base.find("://").map_or(0, |i| i + 3);
	base[authority_start..].find('/').map_or("", |slash| &base[authority_start + slash + 1..]).to_owned()
}

/// Replace the database path segment of a `postgres://user:pass@host:port/db[?p]` URL.
/// Mirrors the signer tests' helper; integration tests are separate crates, so it is
/// copied rather than shared.
fn swap_db(url: &str, name: &str) -> String {
	let (base, query) = match url.split_once('?') {
		Some((base, query)) => (base, Some(query)),
		None => (url, None),
	};
	let authority_start = base.find("://").map_or(0, |i| i + 3);
	let rebuilt = match base[authority_start..].find('/') {
		Some(slash) => format!("{}/{name}", &base[..authority_start + slash]),
		None => format!("{base}/{name}"),
	};
	match query {
		Some(query) => format!("{rebuilt}?{query}"),
		None => rebuilt,
	}
}

/// A pool on this binary's own, already migrated database (see [`database_url`]), or `None`
/// when `DATABASE_URL` is unset — the signal every suite uses to skip on a machine without
/// `nix run .#db`. Under CI the skip becomes a panic. Not cached: each `#[tokio::test]` has
/// its own runtime, and a pool belongs to the runtime that opened it.
pub async fn pool() -> Option<PgPool> {
	let url = database_url().await?;
	let pool = db::connect(&url).await.expect("connect to Postgres");
	Some(pool)
}

fn tigerbeetle_address() -> String {
	std::env::var("TIGERBEETLE_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3033".to_owned())
}

/// A ledger over the configured TigerBeetle replica. Connecting is lazy on the
/// replica's side, so this succeeds even when nothing is listening — call
/// [`seeded_ledger`] when the test needs a replica that actually answers.
pub fn ledger_for(pool: &PgPool) -> Arc<dyn Ledger> {
	let address = tigerbeetle_address();
	let cluster = std::env::var("TIGERBEETLE_CLUSTER_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(0u128);
	let tigerbeetle = Arc::new(TigerBeetle::connect(cluster, &address).expect("connect to TigerBeetle"));
	Arc::new(TbLedger::new(tigerbeetle, pool.clone()))
}

/// A ledger whose singleton accounts exist, or `None` when the replica is unreachable
/// locally (a panic under CI). `skipping` names the suite in the skip notice.
pub async fn seeded_ledger(pool: &PgPool, skipping: &str) -> Option<Arc<dyn Ledger>> {
	let ledger = ledger_for(pool);
	if let Err(err) = ledger::seed_singletons(ledger.as_ref()).await {
		let address = tigerbeetle_address();
		eprintln!("TigerBeetle unreachable at {address} ({err}) — skipping {skipping}");
		return skip_or_fail(&format!("TigerBeetle unreachable at {address} ({err}) — run `nix run .#tb`"));
	}
	Some(ledger)
}

/// No deposit rails at all: no user owns any address, so nothing ever attributes a
/// transfer to a person.
pub struct NoAddresses;

impl domain::architecture::Gateway for NoAddresses {}

#[async_trait]
impl DepositAddresses for NoAddresses {
	async fn address(&self, _user: UserId, _network: Network) -> Result<Option<WalletAddress>, DomainError> {
		Ok(None)
	}
}

/// Mirror a concierge KYC tier onto a local user row.
///
/// Banking deliberately has no aggregate transition for `kyc_level` — the identity plane
/// owns the value and the lifecycle bridge is its only writer — so a test that needs a
/// verified investor sets the column exactly the way the bridge does. `provision` leaves
/// a fresh user at the schema default (tier 0, unverified), which is what the money gates
/// refuse.
pub async fn set_kyc_level(pool: &PgPool, user: UserId, level: i32) {
	let affected = sqlx::query("UPDATE users SET kyc_level = $2 WHERE id = $1")
		.bind(user.raw())
		.bind(level)
		.execute(pool)
		.await
		.expect("mirror the KYC tier")
		.rows_affected();
	assert_eq!(affected, 1, "no user row to mirror the KYC tier onto");
}

/// Write a product's live fee terms straight into `fee_policies` — no code path writes them.
pub async fn install_fee_policy(pool: &PgPool, service: &ServiceId, policy: FeePolicy) {
	sqlx::query(
		"INSERT INTO fee_policies (service, management_bps, performance_bps, hurdle_bps, basis, crystallization, updated_by) VALUES ($1, $2, $3, $4, $5, $6, 'itest') \
		 ON CONFLICT (service) DO UPDATE SET management_bps = EXCLUDED.management_bps, performance_bps = EXCLUDED.performance_bps, hurdle_bps = EXCLUDED.hurdle_bps, \
		   basis = EXCLUDED.basis, crystallization = EXCLUDED.crystallization, version = fee_policies.version + 1, effective_from = now()",
	)
	.bind(service.as_str())
	.bind(i32::try_from(policy.management_bps()).expect("capped by FeePolicy::new"))
	.bind(i32::try_from(policy.performance_bps()).expect("capped by FeePolicy::new"))
	.bind(i32::try_from(policy.hurdle_bps()).expect("capped by FeePolicy::new"))
	.bind(policy.basis().as_str())
	.bind(policy.crystallization().as_str())
	.execute(pool)
	.await
	.expect("install the fee policy");
}
