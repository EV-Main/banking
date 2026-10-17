//! Reads the unit book out of banking for the migration window, read-only.
//!
//! ```text
//! DATABASE_URL=… TIGERBEETLE_ADDRESS=… TIGERBEETLE_CLUSTER_ID=… \
//!   window-export [--out <file>] [--allow-live]
//! ```
//!
//! The file (JSON Lines, schema in `docs/WINDOW-EXPORT.md`) goes to `--out`, which must not
//! exist yet, or to stdout; the Σ of units per allocation goes to stderr. The exit code is
//! non-zero when the export is not clean (a supply that its holders do not add up to, or
//! an outbox that was not drained), so a script cannot carry on past a bad cut.

use std::{fs::OpenOptions, io::BufWriter, process::ExitCode, str::FromStr, sync::Arc};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use piggybank_core::infrastructure::{ledger::TbLedger, tigerbeetle::TigerBeetle, window_export};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

fn env(name: &str) -> Result<String> {
	std::env::var(name).wrap_err_with(|| format!("{name} is not set"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
	color_eyre::install()?;
	let mut out_path = None;
	let mut allow_live = false;
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--out" => out_path = Some(args.next().ok_or_else(|| eyre!("--out needs a path"))?),
			"--allow-live" => allow_live = true,
			other => bail!("unexpected argument `{other}`; usage: window-export [--out <file>] [--allow-live]"),
		}
	}

	// Postgres refuses any write on these sessions, whatever the code below does.
	let options = PgConnectOptions::from_str(&env("DATABASE_URL")?)
		.wrap_err("DATABASE_URL is not a Postgres URL")?
		.options([("default_transaction_read_only", "on")]);
	let pool = PgPoolOptions::new().max_connections(2).connect_with(options).await.wrap_err("connect to Postgres")?;
	let cluster_id = env("TIGERBEETLE_CLUSTER_ID")?;
	let tigerbeetle = Arc::new(TigerBeetle::connect(
		cluster_id.parse().wrap_err("TIGERBEETLE_CLUSTER_ID must be an integer")?,
		&env("TIGERBEETLE_ADDRESS")?,
	)?);
	let ledger = TbLedger::new(tigerbeetle, pool.clone());

	let summary = match out_path {
		Some(path) => {
			// create_new: an export is evidence of one cut, never silently replaced by the next.
			let file = OpenOptions::new().write(true).create_new(true).open(&path).wrap_err_with(|| format!("create {path}"))?;
			window_export::export(&pool, &ledger, &cluster_id, allow_live, BufWriter::new(file)).await?
		}
		None => window_export::export(&pool, &ledger, &cluster_id, allow_live, BufWriter::new(std::io::stdout().lock())).await?,
	};
	eprint!("{}", summary.render());
	Ok(if summary.trailer.clean { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}
