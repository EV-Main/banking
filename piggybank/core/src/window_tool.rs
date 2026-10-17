//! The migration window's operator tool (org restructure plan §6.5), run by hand against the
//! primary while the hub is in maintenance. See `docs/WINDOW-TOOL.md` for the order of a
//! window and `infrastructure::window_tool` for what each step does and guarantees.
//!
//! ```text
//! DATABASE_URL=… TIGERBEETLE_ADDRESS=… TIGERBEETLE_CLUSTER_ID=… window-tool <command>
//!
//!   maintenance on [--yes] | status
//!   drain [--yes] [--wait-secs N] [--release-queued-redemptions]
//!   burn-fee-shares [--yes] [--wait-secs N]
//!   remap [--yes] [--treasury-from-fee <usdt>]
//!   close-share-accounts --export <file> [--yes --import-digest <sha256>]
//!   reconcile [--final]
//!
//! every write (--yes): --expect-cluster-id <id> --anchor-holder <concierge id>:<units> (twice)
//! ```
//!
//! Every command reads the `service_arb` anchor first and last and fails (exit 1) when it is
//! wrong or moved. Every mutating command is a dry run without `--yes`. `maintenance on` is
//! the one write that needs no `--yes`: it only flips the kill-switch.

use std::{process::ExitCode, str::FromStr, sync::Arc, time::Duration};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use piggybank_core::infrastructure::{
	ledger::TbLedger,
	tigerbeetle::TigerBeetle,
	window_tool::{self, Anchor, Mode, Outcome, SingleRun, Window, anchor, burn, close, drain, maintenance, reconcile, remap},
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: window-tool <command>
  maintenance on [--yes] | status
  drain [--yes] [--wait-secs N] [--release-queued-redemptions]
  burn-fee-shares [--yes] [--wait-secs N]
  remap [--yes] [--treasury-from-fee <usdt>]
  close-share-accounts --export <file> [--yes --import-digest <sha256 hex>]
  reconcile [--final]
every write (--yes) also needs: --expect-cluster-id <id>  --anchor-holder <concierge user id>:<units> (twice)";

fn env(name: &str) -> Result<String> {
	std::env::var(name).wrap_err_with(|| format!("{name} is not set"))
}

/// The arguments after the command name, consumed flag by flag so a typo is an error and not
/// a silently ignored `--yes`.
struct Args(Vec<String>);

impl Args {
	fn flag(&mut self, name: &str) -> bool {
		match self.0.iter().position(|a| a == name) {
			Some(i) => {
				self.0.remove(i);
				true
			}
			None => false,
		}
	}

	fn value(&mut self, name: &str) -> Result<Option<String>> {
		let Some(i) = self.0.iter().position(|a| a == name) else {
			return Ok(None);
		};
		self.0.remove(i);
		if i >= self.0.len() {
			bail!("{name} needs a value");
		}
		Ok(Some(self.0.remove(i)))
	}

	fn values(&mut self, name: &str) -> Result<Vec<String>> {
		let mut out = Vec::new();
		while let Some(v) = self.value(name)? {
			out.push(v);
		}
		Ok(out)
	}

	fn seconds(&mut self, name: &str, default: u64) -> Result<Duration> {
		match self.value(name)? {
			Some(raw) => Ok(Duration::from_secs(raw.parse().wrap_err_with(|| format!("{name} must be a whole number of seconds"))?)),
			None => Ok(Duration::from_secs(default)),
		}
	}

	fn finish(self) -> Result<()> {
		if self.0.is_empty() { Ok(()) } else { bail!("unexpected argument(s) {:?}\n{USAGE}", self.0) }
	}
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
	color_eyre::install()?;
	tracing_subscriber::fmt()
		.with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
		.with_writer(std::io::stderr)
		.init();
	let mut argv = std::env::args().skip(1);
	let Some(command) = argv.next() else { bail!("{USAGE}") };
	let mut args = Args(argv.collect());
	let expect_cluster_id = args.value("--expect-cluster-id")?;
	let holders = args.values("--anchor-holder")?;

	let url = env("DATABASE_URL")?;
	let options = PgConnectOptions::from_str(&url).wrap_err("DATABASE_URL is not a Postgres URL")?;
	let cluster_id = env("TIGERBEETLE_CLUSTER_ID")?;
	let tigerbeetle_address = env("TIGERBEETLE_ADDRESS")?;
	// What this run is about to talk to, before it talks: the first line of every command, so
	// a stale environment is seen while it is still harmless. No credentials in it.
	eprintln!(
		"window-tool: postgres {}:{}/{}  tigerbeetle {tigerbeetle_address} cluster {cluster_id}",
		options.get_host(),
		options.get_port(),
		options.get_database().unwrap_or("(default)")
	);
	// One connection for the single-run lock, one for a command's transaction, the rest for
	// the ledger's account-map reads while that transaction is open.
	let pool = PgPoolOptions::new().max_connections(4).connect_with(options).await.wrap_err("connect to Postgres")?;
	let tigerbeetle = Arc::new(TigerBeetle::connect(
		cluster_id.parse().wrap_err("TIGERBEETLE_CLUSTER_ID must be an integer")?,
		&tigerbeetle_address,
	)?);
	let mut anchor = Anchor::service_arb()?;
	if !holders.is_empty() {
		anchor = anchor.with_people(holders.iter().map(|raw| Anchor::parse_holder(raw)).collect::<Result<_>>()?)?;
	}
	let window = Window {
		pool: pool.clone(),
		ledger: Arc::new(TbLedger::new(tigerbeetle, pool)),
		anchor,
	};
	let gate = Gate { cluster_id, expect_cluster_id };

	match dispatch(&window, &gate, &command, args).await {
		Ok(outcome) => {
			print!("{}", outcome.text);
			Ok(if outcome.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
		}
		Err(e) => {
			eprintln!("{e:?}");
			// Whatever went wrong, say where the anchor stands: that is the first thing the
			// operator has to know.
			match anchor::read(&window.pool, window.ledger.as_ref(), &window.anchor).await {
				Ok(reading) => eprintln!("{}", reading.render(&window.anchor)),
				Err(anchor_err) => eprintln!("the anchor could not be read either: {anchor_err:?}"),
			}
			Ok(ExitCode::FAILURE)
		}
	}
}

/// The environment the operator said they mean ([`window_tool::write_gate`]).
struct Gate {
	cluster_id: String,
	expect_cluster_id: Option<String>,
}

impl Gate {
	fn check(&self, w: &Window) -> Result<()> {
		window_tool::write_gate(&w.anchor, &self.cluster_id, self.expect_cluster_id.as_deref())
	}
}

async fn dispatch(w: &Window, gate: &Gate, command: &str, mut args: Args) -> Result<Outcome> {
	match command {
		"maintenance" => {
			let sub = args.0.first().cloned().ok_or_else(|| eyre!("maintenance needs `on` or `status`\n{USAGE}"))?;
			args.0.remove(0);
			match sub.as_str() {
				"on" => {
					let mode = Mode::from_yes(args.flag("--yes"));
					args.finish()?;
					if mode.applies() {
						gate.check(w)?;
					}
					maintenance::on(w, mode).await
				}
				"status" => {
					args.finish()?;
					maintenance::status(w).await
				}
				other => bail!("unknown maintenance subcommand `{other}`\n{USAGE}"),
			}
		}
		"drain" => {
			let mode = Mode::from_yes(args.flag("--yes"));
			let options = drain::Options {
				release_queued_redemptions: args.flag("--release-queued-redemptions"),
				wait: args.seconds("--wait-secs", 120)?,
			};
			args.finish()?;
			let _run = lock_if(w, gate, mode).await?;
			drain::run(w, mode, options).await
		}
		"burn-fee-shares" => {
			let mode = Mode::from_yes(args.flag("--yes"));
			let wait = args.seconds("--wait-secs", 120)?;
			args.finish()?;
			let _run = lock_if(w, gate, mode).await?;
			burn::run(w, mode, wait).await
		}
		"remap" => {
			let mode = Mode::from_yes(args.flag("--yes"));
			let options = remap::Options {
				treasury_from_fee: args.value("--treasury-from-fee")?.map_or(Ok(0), |raw| remap::Options::parse_treasury(&raw))?,
			};
			args.finish()?;
			let _run = lock_if(w, gate, mode).await?;
			remap::run(w, mode, options).await
		}
		"close-share-accounts" => {
			let mode = Mode::from_yes(args.flag("--yes"));
			let path = args.value("--export")?.ok_or_else(|| eyre!("close-share-accounts needs --export <file>\n{USAGE}"))?;
			let import_digest = args.value("--import-digest")?;
			args.finish()?;
			let text = std::fs::read_to_string(&path).wrap_err_with(|| format!("read {path}"))?;
			let export = close::parse_export(&text).wrap_err_with(|| format!("export file {path}"))?;
			let _run = lock_if(w, gate, mode).await?;
			close::run(w, mode, &export, import_digest.as_deref()).await
		}
		"reconcile" => {
			let finalize = args.flag("--final");
			args.finish()?;
			reconcile::run(w, finalize).await
		}
		other => bail!("unknown command `{other}`\n{USAGE}"),
	}
}

/// The write gate and the single-run lock, taken only by a run that writes.
async fn lock_if(w: &Window, gate: &Gate, mode: Mode) -> Result<Option<SingleRun>> {
	if mode.applies() {
		gate.check(w)?;
		Ok(Some(window_tool::SingleRun::acquire(&w.pool).await?))
	} else {
		Ok(None)
	}
}
