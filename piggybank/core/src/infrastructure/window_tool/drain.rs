//! `window-tool drain` — step 1 of the window, after `maintenance on`: leave nothing in
//! flight that the export, the import or the remap would have to guess about.
//!
//! * open book orders are cancelled ([`BookStore::cancel`]) — their escrow goes back to the
//!   owners through the relay;
//! * open consilia are withdrawn in their initiator's name ([`ConsiliumRepository::cancel`]),
//!   the existing terminal state for "this request is no longer valid";
//! * the outbox is waited out: the hub's relay (the tool does not run one) moves every
//!   recorded saga — subscriptions and redemptions included — until nothing is undispatched;
//! * queued redemptions are *reported*. They wait for fund liquidity and cannot be completed
//!   by a drain; releasing one hands the user their units back, so it happens only with
//!   `--release-queued-redemptions`;
//! * consilia that are approved and not executed yet are reported and left: executing one
//!   pays money out, which is not this step's call.
//!
//! The verdict is `drained`: every item above is at zero. What is left is printed whether or
//! not it is, so the operator sees the window's state at a glance.

use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr};
use domain::{book::OrderId, consilium::ConsiliumId, redemptions::RedemptionId, users::UserId};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Mode, Outcome, Window, now_unix, outbox_undispatched, require_maintenance, say};
use crate::{
	infrastructure::{book::PgBook, consilium::PgConsilia, redemptions::PgRedemptions},
	ports::{BookStore, ConsiliumRepository, redemptions::RedemptionRepository},
};

/// How `drain` is asked to behave beyond the defaults.
#[derive(Clone, Copy, Debug)]
pub struct Options {
	/// Release queued redemptions (units back to the user) instead of only reporting them.
	pub release_queued_redemptions: bool,
	/// How long to wait for the relay to empty the outbox. Zero does not wait.
	pub wait: Duration,
}

/// What the window still has to deal with.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Remaining {
	pub open_orders: i64,
	pub queued_redemptions: i64,
	pub open_consilia: i64,
	/// Approved and not yet executed: money-moving, so left alone.
	pub approved_consilia: i64,
	pub outbox_undispatched: i64,
	/// Information, not a gate: parked rows are old business that needs eyes, not a drain.
	pub outbox_parked: i64,
	pub withdrawals_in_flight: i64,
	pub compartment_moves_pending: i64,
}

impl Remaining {
	/// Whether nothing the window depends on is left.
	pub fn drained(&self) -> bool {
		self.open_orders == 0 && self.queued_redemptions == 0 && self.open_consilia == 0 && self.approved_consilia == 0 && self.outbox_undispatched == 0
	}

	pub fn render(&self) -> String {
		let mut out = String::new();
		say!(
			out,
			"left: open orders {}, queued redemptions {}, open consilia {}, approved-not-executed consilia {}, outbox undispatched {}",
			self.open_orders,
			self.queued_redemptions,
			self.open_consilia,
			self.approved_consilia,
			self.outbox_undispatched
		);
		say!(
			out,
			"info: outbox parked {}, withdrawals in flight {}, compartment moves pending {}",
			self.outbox_parked,
			self.withdrawals_in_flight,
			self.compartment_moves_pending
		);
		out
	}
}

async fn count(pool: &PgPool, sql: &'static str) -> Result<i64> {
	sqlx::query_scalar(sql).fetch_one(pool).await.wrap_err_with(|| format!("count: {sql}"))
}

pub async fn remaining(pool: &PgPool) -> Result<Remaining> {
	Ok(Remaining {
		open_orders: count(pool, "SELECT count(*) FROM book_orders WHERE state IN ('open', 'partially_filled')").await?,
		queued_redemptions: count(pool, "SELECT count(*) FROM redemptions WHERE state = 'queued'").await?,
		open_consilia: count(pool, "SELECT count(*) FROM consilium WHERE state = 'open'").await?,
		approved_consilia: count(pool, "SELECT count(*) FROM consilium WHERE state = 'approved'").await?,
		outbox_undispatched: outbox_undispatched(pool).await?,
		outbox_parked: count(pool, "SELECT count(*) FROM outbox WHERE parked_at IS NOT NULL AND compensated_at IS NULL").await?,
		withdrawals_in_flight: count(pool, "SELECT count(*) FROM withdrawals WHERE state IN ('queued', 'processing')").await?,
		compartment_moves_pending: count(pool, "SELECT count(*) FROM compartment_moves WHERE state = 'pending'").await?,
	})
}

/// Wait for the relay to empty the outbox, up to `wait`. Returns what is undispatched.
pub async fn wait_for_outbox(pool: &PgPool, wait: Duration) -> Result<i64> {
	let started = Instant::now();
	loop {
		let n = outbox_undispatched(pool).await?;
		if n == 0 || started.elapsed() >= wait {
			return Ok(n);
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
}

pub async fn run(w: &Window, mode: Mode, options: Options) -> Result<Outcome> {
	let mut out = String::new();
	let before = w.read_anchor().await?;
	say!(out, "{}", before.render(&w.anchor));
	require_maintenance(&w.pool).await?;

	let plan = remaining(&w.pool).await?;
	say!(
		out,
		"plan: cancel {} open order(s), withdraw {} open consilium(s), {} queued redemption(s) {}",
		plan.open_orders,
		plan.open_consilia,
		plan.queued_redemptions,
		if options.release_queued_redemptions { "released" } else { "only reported" }
	);
	if !mode.applies() {
		out.push_str(&plan.render());
		say!(out, "dry run: nothing written (pass --yes to drain)");
		return Ok(Outcome { text: out, ok: plan.drained() });
	}

	let mut failures = Vec::new();

	let book = PgBook::new(w.pool.clone());
	let orders: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM book_orders WHERE state IN ('open', 'partially_filled') ORDER BY seq")
		.fetch_all(&w.pool)
		.await
		.wrap_err("list open orders")?;
	for id in orders {
		// A fill that landed between the list and the cancel is a `Conflict`; the final
		// count below is the verdict, so the order is recorded and the loop goes on.
		if let Err(e) = book.cancel(OrderId::from_raw(id)).await {
			failures.push(format!("order {id}: {e}"));
		}
	}

	let consilia = PgConsilia::new(w.pool.clone());
	let open: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT id, initiator_user_id FROM consilium WHERE state = 'open' ORDER BY created_at")
		.fetch_all(&w.pool)
		.await
		.wrap_err("list open consilia")?;
	for (id, initiator) in open {
		if let Err(e) = consilia.cancel(ConsiliumId::from_raw(id), UserId::from_raw(initiator), now_unix()).await {
			failures.push(format!("consilium {id}: {e}"));
		}
	}

	if options.release_queued_redemptions {
		let redemptions = PgRedemptions::new(w.pool.clone());
		let queued: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM redemptions WHERE state = 'queued' ORDER BY created_at")
			.fetch_all(&w.pool)
			.await
			.wrap_err("list queued redemptions")?;
		for id in queued {
			if let Err(e) = redemptions.fail(RedemptionId::from_raw(id)).await {
				failures.push(format!("redemption {id}: {e}"));
			}
		}
	}

	let undispatched = wait_for_outbox(&w.pool, options.wait).await?;
	say!(out, "outbox: {undispatched} undispatched after waiting up to {}s", options.wait.as_secs());
	for failure in &failures {
		say!(out, "could not close: {failure}");
	}

	let left = remaining(&w.pool).await?;
	out.push_str(&left.render());
	let after = w.read_anchor().await?;
	after.same_holders_as(&before)?;
	say!(out, "{}", after.render(&w.anchor));
	let drained = left.drained();
	say!(out, "{}", if drained { "DRAINED" } else { "NOT DRAINED" });
	Ok(Outcome { text: out, ok: drained })
}
