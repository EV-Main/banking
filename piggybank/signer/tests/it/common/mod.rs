//! Shared test-DB helper: every signer integration test runs against its OWN
//! throwaway database (real Postgres, per the project rules — just not the shared
//! dev one). Keys these tests seal are bound to throwaway KEKs; on the shared dev
//! database they would surface forever as PROVABLY DEAD keys in the KEK-epoch
//! diagnostics and pin its sentinel to a test KEK.

use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use uuid::Uuid;

pub struct TestDb {
	pub pool: PgPool,
	admin_url: String,
	name: String,
}

/// True under CI: `CI` is set, non-empty and not `"0"`/`"false"` (GitHub Actions and the
/// flake's `check-rust` gate both export `CI=true`).
fn ci() -> bool {
	std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// Create `signer_test_<uuid>` on the configured server, migrate it, hand back a pool.
/// `None` (⇒ the caller skips) when no `SIGNER_DATABASE_URL`/`DATABASE_URL` is set — locally
/// only: under CI a missing server panics, so a run that executed no tests cannot go green.
pub async fn throwaway_db() -> Option<TestDb> {
	let url = std::env::var("SIGNER_DATABASE_URL").ok().or_else(|| std::env::var("DATABASE_URL").ok()).filter(|s| !s.is_empty());
	let Some(url) = url else {
		if ci() {
			panic!("integration services required in CI: SIGNER_DATABASE_URL/DATABASE_URL unset — run `nix run .#db` and export DATABASE_URL (or unset CI to skip locally)");
		}
		return None;
	};
	let name = format!("signer_test_{}", Uuid::new_v4().simple());
	let mut admin = PgConnection::connect(&url).await.expect("connect to Postgres");
	// The interpolated identifier is a locally generated hex uuid — not user input.
	sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
		.execute(&mut admin)
		.await
		.expect("create throwaway test database");
	let pool = sqlx::postgres::PgPoolOptions::new()
		.max_connections(2)
		.connect(&swap_db(&url, &name))
		.await
		.expect("connect to the throwaway database");
	sqlx::migrate!().run(&pool).await.expect("apply signer migrations");
	Some(TestDb { pool, admin_url: url, name })
}

impl TestDb {
	/// Best-effort cleanup. A panicked test leaks its `signer_test_*` database —
	/// harmless, and the prefix makes leftovers easy to spot and drop.
	pub async fn cleanup(self) {
		self.pool.close().await;
		if let Ok(mut admin) = PgConnection::connect(&self.admin_url).await {
			let _ = sqlx::query(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name)))
				.execute(&mut admin)
				.await;
		}
	}
}

/// Replace the database path segment of a `postgres://user:pass@host:port/db[?p]` URL.
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
