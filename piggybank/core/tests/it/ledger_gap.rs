//! `ledger-gap` end to end, through its CLI, against two real single-replica clusters:
//! B is a snapshot of A, A keeps committing, and `check` must count exactly what B is
//! missing. Needs the `tigerbeetle` binary on PATH (the dev shell has it).

use std::{
	path::Path,
	process::{Child, Command, Output},
	time::Duration,
};

use tigerbeetle as tb;

const CLUSTER: u128 = 7;
const LEDGER: u32 = 1;
const DEBIT: u128 = 1;
const CREDIT: u128 = 2;

/// A replica that dies with the test, pass or panic.
struct Replica(Child);

impl Replica {
	fn start(file: &Path, port: u16) -> Self {
		Self(
			Command::new("tigerbeetle")
				.args(["start", &format!("--addresses=127.0.0.1:{port}"), "--cache-grid=64MiB"])
				.arg(file)
				.stderr(std::process::Stdio::null())
				.spawn()
				.expect("tigerbeetle start"),
		)
	}
}

impl Drop for Replica {
	fn drop(&mut self) {
		self.0.kill().expect("kill replica");
		self.0.wait().expect("reap replica");
	}
}

fn free_port() -> u16 {
	std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn ledger_gap(port: u16, args: &[&str]) -> Output {
	let (command, rest) = args.split_first().unwrap();
	Command::new(env!("CARGO_BIN_EXE_ledger-gap"))
		.args(["--cluster-id", &CLUSTER.to_string(), command, "--addresses", &format!("127.0.0.1:{port}")])
		.args(rest)
		.output()
		.expect("run ledger-gap")
}

fn stdout(output: &Output) -> String {
	String::from_utf8(output.stdout.clone()).unwrap().trim().to_owned()
}

fn transfer(id: u128) -> tb::Transfer {
	tb::Transfer {
		id,
		debit_account_id: DEBIT,
		credit_account_id: CREDIT,
		amount: id * 10,
		ledger: LEDGER,
		code: 1,
		..Default::default()
	}
}

async fn commit(port: u16, transfers: &[tb::Transfer]) {
	let client = tb::Client::new(CLUSTER, &format!("127.0.0.1:{port}")).unwrap();
	let created = tokio::time::timeout(Duration::from_secs(60), client.create_transfers(transfers).unwrap())
		.await
		.expect("cluster answers")
		.unwrap();
	assert!(created.iter().all(|r| r.status == tb::CreateTransferStatus::Created), "{created:?}");
}

#[tokio::test]
async fn check_counts_exactly_what_the_snapshot_is_missing() {
	if Command::new("tigerbeetle").arg("version").output().is_err() {
		assert!(std::env::var("CI").is_err(), "tigerbeetle binary required in CI");
		eprintln!("no tigerbeetle on PATH — skipping ledger-gap test");
		return;
	}
	// Not /tmp: TigerBeetle wants O_DIRECT, which tmpfs refuses.
	let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ledger-gap-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	let (a, b) = (dir.join("a.tigerbeetle"), dir.join("b.tigerbeetle"));
	let format = Command::new("tigerbeetle")
		.args(["format", &format!("--cluster={CLUSTER}"), "--replica=0", "--replica-count=1"])
		.arg(&a)
		.output()
		.unwrap();
	assert!(format.status.success(), "{}", String::from_utf8_lossy(&format.stderr));
	let (pa, pb) = (free_port(), free_port());

	{
		let _a = Replica::start(&a, pa);
		let client = tb::Client::new(CLUSTER, &format!("127.0.0.1:{pa}")).unwrap();
		let accounts: Vec<_> = [DEBIT, CREDIT]
			.map(|id| tb::Account {
				id,
				ledger: LEDGER,
				code: 1,
				..Default::default()
			})
			.into();
		let created = tokio::time::timeout(Duration::from_secs(60), client.create_accounts(&accounts).unwrap())
			.await
			.expect("cluster answers")
			.unwrap();
		assert!(created.iter().all(|r| r.status == tb::CreateAccountStatus::Created), "{created:?}");
		commit(pa, &(100..105).map(transfer).collect::<Vec<_>>()).await;
	}
	std::fs::copy(&a, &b).unwrap();
	let _a = Replica::start(&a, pa);
	let _b = Replica::start(&b, pb);
	let after: Vec<_> = (200..203).map(transfer).collect();
	commit(pa, &after).await;

	let watermark = ledger_gap(pb, &["watermark"]);
	assert!(watermark.status.success(), "{}", String::from_utf8_lossy(&watermark.stderr));
	let watermark = stdout(&watermark);
	// The copy is gone, only A is left: the snapshot's watermark is A's newest object below
	// the first thing committed after it.
	let first_after = tb::Client::new(CLUSTER, &format!("127.0.0.1:{pa}")).unwrap();
	let first_after = tokio::time::timeout(Duration::from_secs(60), first_after.lookup_transfers(&[after[0].id]).unwrap())
		.await
		.unwrap()
		.unwrap()[0]
		.timestamp;
	let bounded = ledger_gap(pa, &["watermark", "--before", &first_after.to_string()]);
	assert_eq!(stdout(&bounded), watermark, "{}", String::from_utf8_lossy(&bounded.stderr));
	let gap = dir.join("gap.json");
	let export = ledger_gap(pa, &["export", "--since", &watermark, "--out", gap.to_str().unwrap()]);
	assert!(export.status.success(), "{}", String::from_utf8_lossy(&export.stderr));

	let check = |expect_ok: bool, expected: &str| {
		let out = ledger_gap(pb, &["check", "--gap", gap.to_str().unwrap()]);
		assert_eq!(out.status.success(), expect_ok, "{}", String::from_utf8_lossy(&out.stderr));
		let line = stdout(&out);
		assert!(line.starts_with(expected), "{line}");
	};
	check(false, "3 transfers (Σ amount per ledger: 1=6030) and 0 accounts missing, 0 mismatched since");
	commit(pb, &after[..1]).await;
	check(false, "2 transfers (Σ amount per ledger: 1=4030) and 0 accounts missing, 0 mismatched since");
	commit(pb, &after[1..]).await;
	check(true, "all 3 transfers and 0 accounts since");

	let tampered = ledger_gap(pa, &["export", "--since", "0", "--out", gap.to_str().unwrap()]);
	assert!(tampered.status.success());
	let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&gap).unwrap()).unwrap();
	file["transfers"][0]["amount"] = "1".into();
	std::fs::write(&gap, file.to_string()).unwrap();
	check(false, "0 transfers (Σ amount per ledger: ) and 0 accounts missing, 1 mismatched since");
	std::fs::remove_dir_all(&dir).unwrap();
}
