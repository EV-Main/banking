//! The migration set's own invariant, checked without a database: one version, one file.
//!
//! `_sqlx_migrations.version` is that table's primary key, so two migration files numbered
//! alike can never both be recorded. The migrator does not notice up front — it applies the
//! lower file, records the version, then fails the next one's INSERT with `23505` on
//! `_sqlx_migrations_pkey` against a database it has already half-migrated. Nothing about
//! that error names the real cause, and the damage does not stop at the failing run: the
//! recorded row now carries the *first* file's checksum, so the next migration of the same
//! database reports `migration N was previously applied but has been modified`, which the
//! test harness reads as another branch's template and answers by dropping and rebuilding
//! it — straight back into the duplicate INSERT (#434).
//!
//! That is a collision two branches can create without either one touching the other's
//! files, and it takes down every suite at once rather than the change that caused it. One
//! `#[test]` with no services behind it names it in the words of the fix instead.

use std::collections::BTreeMap;

/// Given this build's embedded migration set — the same one `db::migrate` applies at service
/// start-up and the test harness applies to its template — when the versions are collected,
/// then no two migrations share one.
#[test]
fn every_migration_has_a_version_of_its_own() {
	let mut by_version: BTreeMap<i64, Vec<&str>> = BTreeMap::new();
	for migration in sqlx::migrate!().iter() {
		by_version.entry(migration.version).or_default().push(&migration.description);
	}

	let collisions: Vec<String> = by_version
		.iter()
		.filter(|(_, descriptions)| descriptions.len() > 1)
		.map(|(version, descriptions)| format!("{version:04} is shared by {} migrations: {}", descriptions.len(), descriptions.join(" | ")))
		.collect();

	assert!(
		collisions.is_empty(),
		"expected every migration version in piggybank/core/migrations to appear exactly once, got {} collision(s) — renumber the one that has NOT been released yet (`git tag --contains` the commit that added it) and keep the files in dependency order:\n  {}",
		collisions.len(),
		collisions.join("\n  ")
	);
}
