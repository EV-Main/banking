// The single, central registry of UX tips: which anchors exist, how each one renders,
// and who is allowed to see it. The rendering engine (@evinvest/uikit InfoTip /
// SectionDescriptor) stays content-agnostic and never sees any copy until
// <TipAnchor> resolves a key.
//
// The English is here, beside the key it is filed under: `title` and `body` are thunks
// over the translator, so `messages/<locale>/common.json` is generated out of THIS file
// rather than being the place the copy is edited. Thunks and not strings because the
// catalog is a module-scope const and `t` is a hook — this way <TipAnchor> resolves the
// one entry it renders instead of every entry being formatted on every render.
//
// A tip is prose a reader reads, so it is translated like every other sentence in the
// cabinet; `type` picks the primitive and `roles` gates the render, and both are decided
// at compile time.
//
// Authored as a typed `const` (not JSON) so every anchor id is a compile-time literal: a
// <TipAnchor anchor="…"> referencing a key that does not exist here fails `tsc`. Bodies
// are plain text (no markup) — the bubble is a non-interactive help hint, so there are
// no links to render.
//
// Keys are dot-namespaced by surface (`wallet.*`, `invest.*`, `admin.<area>.*`).
// Investor surfaces are ungated; admin surfaces gate to OPS so operator jargon
// never renders for investors (cosmetic only — server authz stays authoritative).
// Money-safety strings are lifted verbatim from the views so a tip can never drift from
// real behaviour; both halves of such a pair are literals in this repo again, so keep
// them in step.

import type { Translate } from "@evinvest/i18n";

export type TipType = "input" | "section";

export interface TipEntry {
  /** Which primitive renders this tip: an inline ⓘ toggletip, or a section block. */
  type: TipType;
  /**
   * Optional platform-role gate. When set, only sessions whose role is listed
   * see the tip. Cosmetic only — server-side authorization stays authoritative.
   */
  roles?: readonly string[];
  /** The tip's heading, in the reader's language. */
  title: (t: Translate) => string;
  /** The explanation under it. Plain text. */
  body: (t: Translate) => string;
}

export type TipCatalog = Record<string, TipEntry>;

/** Operator-facing surfaces — hidden from investors. */
const OPS = ["operator", "admin", "owner"] as const;

export const tips = {
  // ── wallet ──────────────────────────────────────────────────────────────────
  "wallet.balance.model": { type: "section",
		body: t => t("tips.wallet.balance.model.body", "You have a single USDT balance — networks are just how you deposit and withdraw. This card splits it into Total = Available + Invested + Pending withdrawal."),
		title: t => t("tips.wallet.balance.model.title", "One balance, many rails") },
  "wallet.balance.available": { type: "input",
		body: t => t("tips.wallet.balance.available.body", "Your spendable USDT right now — what you can invest into fund units or withdraw."),
		title: t => t("tips.wallet.balance.available.title", "Available") },
  "wallet.balance.invested": { type: "input",
		body: t => t("tips.wallet.balance.invested.body", "Held in fund units, valued at the current NAV."),
		title: t => t("tips.wallet.balance.invested.title", "Invested") },
  "wallet.balance.pending-withdrawal": { type: "input",
		body: t => t("tips.wallet.balance.pending-withdrawal.body", "USDT reserved for withdrawals still in flight (queued or processing). It is no longer part of your available balance."),
		title: t => t("tips.wallet.balance.pending-withdrawal.title", "Pending withdrawal") },
  "wallet.deposit.network": { type: "input",
		body: t => t("tips.wallet.deposit.network.body", "Pick the chain you'll send USDT on. Each rail shows its own deposit address below — that address only works on that one chain."),
		title: t => t("tips.wallet.deposit.network.title", "Network / rail") },
  "wallet.deposit.address": { type: "input",
		body: t => t("tips.wallet.deposit.address.body", "A personal, reusable address for this rail. On TON it is shown in the wallet-friendly UQ… form so an uninitialised wallet doesn't bounce the transfer."),
		title: t => t("tips.wallet.deposit.address.title", "Your deposit address") },
  "wallet.deposit.min-confirmations": { type: "input",
		body: t => t("tips.wallet.deposit.min-confirmations.body", "Credited to your one balance after the required number of network confirmations."),
		title: t => t("tips.wallet.deposit.min-confirmations.title", "Network confirmations") },
  "wallet.deposit.rail-hazard": { type: "section",
		body: t => t("tips.wallet.deposit.rail-hazard.body", "Sending any other asset, or using a different network, loses the funds permanently. This 0x address is NOT your address on the sibling EVM chain — even though both start with 0x, USDT sent on the wrong EVM rail will not be credited."),
		title: t => t("tips.wallet.deposit.rail-hazard.title", "Send only USDT on this rail") },
  "wallet.withdraw.network": { type: "input",
		body: t => t("tips.wallet.withdraw.network.body", "The chain your withdrawal ships on. The destination address must be a valid address on this exact network, or the funds are lost."),
		title: t => t("tips.wallet.withdraw.network.title", "Network / rail") },
  "wallet.withdraw.destination": { type: "input",
		body: t => t("tips.wallet.withdraw.destination.body", "The on-chain address the USDT is sent to. Withdrawals are irreversible — make sure it is correct and on the network you selected."),
		title: t => t("tips.wallet.withdraw.destination.title", "Destination address") },
  "wallet.withdraw.available": { type: "input",
		body: t => t("tips.wallet.withdraw.available.body", "The most you can withdraw on this rail right now. Tap Max to fill the amount."),
		title: t => t("tips.wallet.withdraw.available.title", "Available to withdraw") },
  "wallet.withdraw.network-fee": { type: "input",
		body: t => t("tips.wallet.withdraw.network-fee.body", "A flat on-chain fee for this rail, deducted from your amount before it is sent."),
		title: t => t("tips.wallet.withdraw.network-fee.title", "Network fee") },
  "wallet.withdraw.you-receive": { type: "input",
		body: t => t("tips.wallet.withdraw.you-receive.body", "Your amount minus the network fee — this is what actually lands at the destination."),
		title: t => t("tips.wallet.withdraw.you-receive.title", "You will receive") },
  "wallet.withdraw.queueing": { type: "section",
		body: t => t("tips.wallet.withdraw.queueing.body", "Up to your instant limit pays out immediately on this rail; anything above it is queued until the rail is topped up. A minimum applies per withdrawal."),
		title: t => t("tips.wallet.withdraw.queueing.title", "How withdrawals work") },
  "wallet.withdraw.review": { type: "section",
		body: t => t("tips.wallet.withdraw.review.body", "Review freezes exactly what you'll send. Confirm submits that snapshot — if the rail list changes underneath, the open review is voided and you re-review."),
		title: t => t("tips.wallet.withdraw.review.title", "Review, then confirm") },

  // ── invest ──────────────────────────────────────────────────────────────────
  "invest.overview": { type: "section",
		body: t => t("tips.invest.overview.body", "Subscribe USDT for units — your value tracks the fund's NAV, not the unit count."),
		title: t => t("tips.invest.overview.title", "Fund shares") },
  "invest.position.units": { type: "input",
		body: t => t("tips.invest.position.units.body", "Your share count in the fund's pool — a fixed quantity you own until you subscribe or redeem more."),
		title: t => t("tips.invest.position.units.title", "Units") },
  "invest.position.nav": { type: "input",
		body: t => t("tips.invest.position.nav.body", "Net asset value per unit — the current price at which units are bought on subscribe and priced on redeem."),
		title: t => t("tips.invest.position.nav.title", "NAV") },
  "invest.position.value": { type: "input",
		body: t => t("tips.invest.position.value.body", "Units × current NAV. It rises and falls as the NAV moves, not with the unit count."),
		title: t => t("tips.invest.position.value.title", "Value") },
  "invest.position.stale-nav": { type: "input",
		body: t => t("tips.invest.position.stale-nav.body", "The fund's price feed is out of date, so the value and P&L shown here may lag the true mark until the NAV refreshes."),
		title: t => t("tips.invest.position.stale-nav.title", "Stale NAV") },
  "invest.position.pnl": { type: "input",
		body: t => t("tips.invest.position.pnl.body", "Your position's current value minus what you paid in — green for a gain, red for a loss, driven by the NAV moving."),
		title: t => t("tips.invest.position.pnl.title", "Profit & loss") },
  "invest.subscribe.amount": { type: "input",
		body: t => t("tips.invest.subscribe.amount.body", "How much USDT to subscribe. It is converted into units at the current NAV when you submit."),
		title: t => t("tips.invest.subscribe.amount.title", "Amount") },
  "invest.redeem.units": { type: "input",
		body: t => t("tips.invest.redeem.units.body", "How many units to redeem — Max fills your full holding. The resulting cash isn't fixed here; it is set at the settle-time NAV."),
		title: t => t("tips.invest.redeem.units.title", "Units to redeem") },
  "invest.redeem.queue": { type: "section",
		body: t => t("tips.invest.redeem.queue.body", "Redemptions are accept-and-queue — your units are reserved now, and cash is priced at the settle-time NAV once the fund tops up."),
		title: t => t("tips.invest.redeem.queue.title", "How redemptions settle") },
  "invest.activity.status": { type: "input",
		body: t => t("tips.invest.activity.status.body", "Queued (reserved, awaiting settle), completed (paid out at the settle NAV), failed, or cancelled."),
		title: t => t("tips.invest.activity.status.title", "Redemption status") },
  "invest.activity.cancel": { type: "input",
		body: t => t("tips.invest.activity.cancel.body", "Available only while a redemption is still queued. It withdraws the request and releases the reserved units back to your holding before any settle NAV is applied."),
		title: t => t("tips.invest.activity.cancel.title", "Cancel") },

  // ── dashboard ───────────────────────────────────────────────────────────────
  "dashboard.performance.portfolio-value": { type: "input",
		body: t => t("tips.dashboard.performance.portfolio-value.body", "Your whole balance: available + invested + pending withdrawal. Money locked in a queued withdrawal is still counted here."),
		title: t => t("tips.dashboard.performance.portfolio-value.title", "Portfolio value") },
  "dashboard.performance.all-time-return": { type: "input",
		body: t => t("tips.dashboard.performance.all-time-return.body", "Total unrealized P&L divided by what you put in at cost basis, across every position. A paper figure, not realized cash."),
		title: t => t("tips.dashboard.performance.all-time-return.title", "All-time return") },
  "dashboard.performance.series": { type: "section",
		body: t => t("tips.dashboard.performance.series.body", "'Fund performance' is the fund's own return; 'Your participation' is how your stake performed, which differs by when you contributed. The live series isn't wired up yet."),
		title: t => t("tips.dashboard.performance.series.title", "Performance chart") },
  "dashboard.move-money.auto-deploy": { type: "input",
		body: t => t("tips.dashboard.move-money.auto-deploy.body", "When on, your available cash is automatically committed into strategies at end of day."),
		title: t => t("tips.dashboard.move-money.auto-deploy.title", "Auto-deploy idle cash") },
  "dashboard.invested.allocation": { type: "input",
		body: t => t("tips.dashboard.invested.allocation.body", "Each bar is one strategy's share of your invested value at current NAV. Percentages are of invested value only and exclude your available cash."),
		title: t => t("tips.dashboard.invested.allocation.title", "Invested — what I own") },
  "dashboard.stats.unrealized-pnl": { type: "input",
		body: t => t("tips.dashboard.stats.unrealized-pnl.body", "Paper gain or loss across all positions — current value minus cost basis. It becomes realized cash only when you redeem."),
		title: t => t("tips.dashboard.stats.unrealized-pnl.title", "Unrealized P&L") },
  "dashboard.stats.available": { type: "input",
		body: t => t("tips.dashboard.stats.available.body", "Cash that is free and spendable now — ready to deploy into a strategy or withdraw."),
		title: t => t("tips.dashboard.stats.available.title", "Available") },
  "dashboard.stats.net-invested": { type: "input",
		body: t => t("tips.dashboard.stats.net-invested.body", "Total cash you put into strategies at cost basis, net of redemptions — not the current value."),
		title: t => t("tips.dashboard.stats.net-invested.title", "Net invested") },

  // ── settings ────────────────────────────────────────────────────────────────
  "settings.security.google-signin": { type: "section",
		body: t => t("tips.settings.security.google-signin.body", "Your sign-in and password are managed by Google. Two-factor authentication and recovery are configured in your Google Account."),
		title: t => t("tips.settings.security.google-signin.title", "Sign-in is managed by Google") },
  "settings.sessions.overview": { type: "section",
		body: t => t("tips.settings.sessions.overview.body", "Where you're signed in — each row is a device or browser with a live session. Revoke anything you don't recognise."),
		title: t => t("tips.settings.sessions.overview.title", "Sessions & devices") },
  "settings.sessions.this-device": { type: "input",
		body: t => t("tips.settings.sessions.this-device.body", "The browser you're using right now. It stays signed in and can't be revoked from here — use 'Sign out all other devices' to clear the rest."),
		title: t => t("tips.settings.sessions.this-device.title", "This device") },
  "settings.sessions.revoke": { type: "input",
		body: t => t("tips.settings.sessions.revoke.body", "Signs that device out immediately. It must sign in again with Google to regain access."),
		title: t => t("tips.settings.sessions.revoke.title", "Revoke") },
  "settings.sessions.revoke-others": { type: "input",
		body: t => t("tips.settings.sessions.revoke-others.body", "Signs out every device except this one. Use it if a device was lost or you don't recognise a session."),
		title: t => t("tips.settings.sessions.revoke-others.title", "Sign out all other devices") },

  // ── profile ─────────────────────────────────────────────────────────────────
  "profile.personal.compliance": { type: "section",
		body: t => t("tips.profile.personal.compliance.body", "These personal details are collected to meet the fund's compliance obligations and to produce your account statements. They are not shown publicly."),
		title: t => t("tips.profile.personal.compliance.title", "Why we collect this") },
  "profile.field.legal-name": { type: "input",
		body: t => t("tips.profile.field.legal-name.body", "Your full name exactly as it appears on official documents — used for compliance and statements, distinct from your preferred name."),
		title: t => t("tips.profile.field.legal-name.title", "Legal name") },
  "profile.field.nationality": { type: "input",
		body: t => t("tips.profile.field.nationality.body", "Collected for compliance and eligibility checks."),
		title: t => t("tips.profile.field.nationality.title", "Nationality") },
  "profile.field.tax-residence": { type: "input",
		body: t => t("tips.profile.field.tax-residence.body", "The country where you're liable to pay tax, used for tax reporting. Usually where you live — it can differ from your nationality."),
		title: t => t("tips.profile.field.tax-residence.title", "Tax residence") },
  "profile.email.verified": { type: "input",
		body: t => t("tips.profile.email.verified.body", "Confirms this email address has been verified. It is not identity or KYC verification."),
		title: t => t("tips.profile.email.verified.title", "Verified") },

  // ── admin · users ───────────────────────────────────────────────────────────
  "admin.users.access.role": { type: "input", roles: OPS,
		body: t => t("tips.admin.users.access.role.body", "The access level for this user: investor, operator or admin. Raising it grants operator/admin console access immediately on change. Owner is not here — a seat is the consilium's to give and to take away."),
		title: t => t("tips.admin.users.access.role.title", "Role") },
  "admin.users.access.kyc-level": {
    // Per-level gating is defined by the KYC/money-plane policy, not in the view —
    // confirm the exact tier→limit mapping with policy before shipping.
    type: "input",
    roles: OPS,
		body: t => t("tips.admin.users.access.kyc-level.body", "The user's identity-verification tier. Higher levels unlock higher limits and actions per the KYC policy."),
		title: t => t("tips.admin.users.access.kyc-level.title", "KYC level"),
  },
  "admin.users.access.revoke-sessions": { type: "input", roles: OPS,
		body: t => t("tips.admin.users.access.revoke-sessions.body", "Bumps token_version — invalidates every JWT issued to this user, signing them out of every active session."),
		title: t => t("tips.admin.users.access.revoke-sessions.title", "Revoke all sessions") },
  "admin.users.identity.token-version": { type: "input", roles: OPS,
		body: t => t("tips.admin.users.identity.token-version.body", "A per-user counter stamped into every issued JWT. It increments on 'Revoke all sessions', so any token carrying an older version fails verification."),
		title: t => t("tips.admin.users.identity.token-version.title", "Token version") },
  "admin.users.status.suspend": { type: "input", roles: OPS,
		body: t => t("tips.admin.users.status.suspend.body", "Suspend disables the account and blocks the user's access; Reinstate re-enables it. Reversible — the button reflects the current status."),
		title: t => t("tips.admin.users.status.suspend.title", "Suspend / Reinstate") },

  // ── admin · overview ────────────────────────────────────────────────────────
  "admin.overview.kpi.parked-rows": { type: "input", roles: OPS,
		body: t => t("tips.admin.overview.kpi.parked-rows.body", "Money the relay couldn't apply — outbox rows stuck until unparked. Nonzero turns the tile red."),
		title: t => t("tips.admin.overview.kpi.parked-rows.title", "Parked rows") },
  "admin.overview.kpi.dispatch-backlog": { type: "input", roles: OPS,
		body: t => t("tips.admin.overview.kpi.dispatch-backlog.body", "Undispatched outbox rows — events written but not yet relayed. A healthy relay keeps this near zero."),
		title: t => t("tips.admin.overview.kpi.dispatch-backlog.title", "Dispatch backlog") },
  "admin.overview.kpi.oldest-backlog": { type: "input", roles: OPS,
		body: t => t("tips.admin.overview.kpi.oldest-backlog.body", "Age of the oldest undispatched row. A growing age means the relay is stalled or falling behind."),
		title: t => t("tips.admin.overview.kpi.oldest-backlog.title", "Oldest backlog") },
  "admin.overview.kpi.dead-key-signings": { type: "input", roles: OPS,
		body: t => t("tips.admin.overview.kpi.dead-key-signings.body", "The signer couldn't unseal a wallet key (wrong or rotated KEK epoch), so those funds can't be moved. Nonzero is a money-safety alarm — page an operator."),
		title: t => t("tips.admin.overview.kpi.dead-key-signings.title", "Dead-key signings") },
  "admin.overview.parked-events": { type: "section", roles: OPS,
		body: t => t("tips.admin.overview.parked-events.body", "A row parks when the relay hits a terminal apply error. Fix the cause shown in the Reason column first, then unpark to re-drive it — otherwise it just re-parks."),
		title: t => t("tips.admin.overview.parked-events.title", "Parked events") },
  "admin.overview.parked.reason": { type: "input", roles: OPS,
		body: t => t("tips.admin.overview.parked.reason.body", "The relay's failure cause for this parked row — the thing you must fix before unparking."),
		title: t => t("tips.admin.overview.parked.reason.title", "Reason") },
  "admin.overview.parked.compensated": { type: "input", roles: OPS,
		body: t => t("tips.admin.overview.parked.compensated.body", "This row's saga was already reversed, so its money effect is undone. Unpark is disabled to avoid re-applying an entry that was intentionally rolled back."),
		title: t => t("tips.admin.overview.parked.compensated.title", "Compensated") },
  "admin.overview.parked.unpark": { type: "input", roles: OPS,
		body: t => t("tips.admin.overview.parked.unpark.body", "Re-drives this outbox row through the relay after you've fixed its cause. Disabled when the row was compensated, already unparked this session, or while another unpark is in flight."),
		title: t => t("tips.admin.overview.parked.unpark.title", "Unpark") },

  // ── admin · treasury ────────────────────────────────────────────────────────
  "admin.treasury.two-layer-model": { type: "section", roles: OPS,
		body: t => t("tips.admin.treasury.two-layer-model.body", "Layer 1 is the ledger's network-agnostic USDT claims; Layer 2 is the actual on-chain liquidity held per rail. The two must reconcile."),
		title: t => t("tips.admin.treasury.two-layer-model.title", "Two layers") },
  "admin.treasury.layer1.ledger": { type: "section", roles: OPS,
		body: t => t("tips.admin.treasury.layer1.ledger.body", "The ledger's network-agnostic claims in USDT. Total claims equal on-chain custody and break down into held-for-clients, fund capital, and reserved-for-withdrawals."),
		title: t => t("tips.admin.treasury.layer1.ledger.title", "Layer 1 · Ledger") },
  "admin.treasury.layer1.claims-total": { type: "input", roles: OPS,
		body: t => t("tips.admin.treasury.layer1.claims-total.body", "Total user + fund claims. Equals the sum of on-chain custody — every claim is fully backed."),
		title: t => t("tips.admin.treasury.layer1.claims-total.title", "Claims · total") },
  "admin.treasury.layer1.held-for-clients": { type: "input", roles: OPS,
		body: t => t("tips.admin.treasury.layer1.held-for-clients.body", "The portion of claims owed to clients — user wallet plus service balances."),
		title: t => t("tips.admin.treasury.layer1.held-for-clients.title", "Held for clients") },
  "admin.treasury.layer1.fund-capital": { type: "input", roles: OPS,
		body: t => t("tips.admin.treasury.layer1.fund-capital.body", "Claims that are the fund's own capital, not client money."),
		title: t => t("tips.admin.treasury.layer1.fund-capital.title", "Fund capital") },
  "admin.treasury.layer1.reserved-withdrawals": { type: "input", roles: OPS,
		body: t => t("tips.admin.treasury.layer1.reserved-withdrawals.body", "Claims set aside for queued and in-flight withdrawals, parked in the clearing account until they settle on-chain."),
		title: t => t("tips.admin.treasury.layer1.reserved-withdrawals.title", "Reserved · withdrawals") },
  "admin.treasury.layer2.rails": { type: "section", roles: OPS,
		body: t => t("tips.admin.treasury.layer2.rails.body", "The actual on-chain USDT liquidity held per rail (BEP20 / TRC20 / TON / Polygon), plus the fiat bank balance. This is where the backing physically sits."),
		title: t => t("tips.admin.treasury.layer2.rails.title", "Layer 2 · Treasury") },
  "admin.treasury.rail.funding": { type: "section", roles: OPS,
		body: t => t("tips.admin.treasury.rail.funding.body", "The card value is this rail's ledger custody; the footer shows the hot wallet's real on-chain USDT and native gas. The two can legitimately diverge under accept-and-queue."),
		title: t => t("tips.admin.treasury.rail.funding.title", "Rail funding") },
  "admin.treasury.rail.address": { type: "input", roles: OPS,
		body: t => t("tips.admin.treasury.rail.address.body", "This rail's on-chain hot-wallet address. BEP20 and Polygon share an identical EVM address format — topping up on the wrong EVM chain, or sending another rail's funds here, is an irreversible mis-send."),
		title: t => t("tips.admin.treasury.rail.address.title", "Treasury address") },
  "admin.treasury.rail.gas-station": { type: "input", roles: OPS,
		body: t => t("tips.admin.treasury.rail.gas-station.body", "Top this address up with the rail's native token to fund sweep gas. Send the correct native symbol on the correct chain — EVM gas-station addresses are shared, so a wrong-chain top-up is unrecoverable."),
		title: t => t("tips.admin.treasury.rail.gas-station.title", "Gas station") },
  "admin.treasury.bank": { type: "input", roles: OPS,
		body: t => t("tips.admin.treasury.bank.body", "The fiat USD bank balance used for off-ramp and FX, counted as treasury liquidity alongside the on-chain rails."),
		title: t => t("tips.admin.treasury.bank.title", "Bank · USD") },
  "admin.treasury.invariant": { type: "section", roles: OPS,
		body: t => t("tips.admin.treasury.invariant.body", "Per-rail backing is the treasury's job, not the ledger's: a shortfall on one rail is accept-and-queue, then rebalanced via CEX, alt-rail, or top-up. The global invariant is sum(custody) == sum(claims)."),
		title: t => t("tips.admin.treasury.invariant.title", "The invariant") },

  // ── admin · valuation ───────────────────────────────────────────────────────
  "admin.valuation.post.aum": { type: "input", roles: OPS,
		body: t => t("tips.admin.valuation.post.aum.body", "The fund's total assets under management you're marking. The NAV/share that pays redemptions is derived from it (AUM ÷ units outstanding), so a wrong figure mis-prices every settle."),
		title: t => t("tips.admin.valuation.post.aum.title", "AUM (USDT)") },
  "admin.valuation.post.derived-nav": { type: "input", roles: OPS,
		body: t => t("tips.admin.valuation.post.derived-nav.body", "Entered AUM divided by units outstanding. This is the mark that Post valuation commits."),
		title: t => t("tips.admin.valuation.post.derived-nav.title", "Derived NAV / share") },
  "admin.valuation.post.nav-guard": { type: "section", roles: OPS,
		body: t => t("tips.admin.valuation.post.nav-guard.body", "A post is rejected if the NAV moves more than 50% from the last mark, unless Override guard is on."),
		title: t => t("tips.admin.valuation.post.nav-guard.title", "NAV-move guard") },
  "admin.valuation.post.override": { type: "input", roles: OPS,
		body: t => t("tips.admin.valuation.post.override.body", "Bypasses the NAV-move guard so a post that moves NAV more than 50% is accepted and settles redemptions at that mark. A fat-fingered AUM then over- or under-pays irreversibly."),
		title: t => t("tips.admin.valuation.post.override.title", "Override guard") },
  "admin.valuation.queue.settle-fail": { type: "section", roles: OPS,
		body: t => t("tips.admin.valuation.queue.settle-fail.body", "Settle pays at settle-time NAV once the fund claim is liquid; if the rail is short the payout queues until treasury tops up. Fail voids the request and refunds the units."),
		title: t => t("tips.admin.valuation.queue.settle-fail.title", "Settle vs Fail") },
  "admin.valuation.queue.est-cash": { type: "input", roles: OPS,
		body: t => t("tips.admin.valuation.queue.est-cash.body", "A preview only — approximately units × current NAV. The actual payout settles at the settle-time NAV, not this figure."),
		title: t => t("tips.admin.valuation.queue.est-cash.title", "Est. cash") },
  "admin.valuation.queue.settle": { type: "input", roles: OPS,
		body: t => t("tips.admin.valuation.queue.settle.body", "Pays at the settle-time NAV once the fund claim is liquid, queuing if the rail is short. Settle also burns the user's units."),
		title: t => t("tips.admin.valuation.queue.settle.title", "Settle") },
  "admin.valuation.queue.fail": { type: "input", roles: OPS,
		body: t => t("tips.admin.valuation.queue.fail.body", "Voids the request and refunds the units — no cash is paid and the burned units are returned to the user."),
		title: t => t("tips.admin.valuation.queue.fail.title", "Fail") },

  // ── admin · withdrawals ─────────────────────────────────────────────────────
  "admin.withdrawals.flow": { type: "section", roles: OPS,
		body: t => t("tips.admin.withdrawals.flow.body", "Dispatch broadcasts a queued withdrawal once its rail has liquidity. Settle records the mined transaction and releases the reservation. Fail voids and refunds — only safe when nothing reached the chain."),
		title: t => t("tips.admin.withdrawals.flow.title", "Dispatch, settle, fail") },
  "admin.withdrawals.gross-net": { type: "input", roles: OPS,
		body: t => t("tips.admin.withdrawals.gross-net.body", "Gross is the full amount debited from the user; net is what is actually sent on-chain after the withdrawal fee."),
		title: t => t("tips.admin.withdrawals.gross-net.title", "Gross / net") },
  "admin.withdrawals.state": { type: "input", roles: OPS,
		body: t => t("tips.admin.withdrawals.state.body", "Queued = accepted and reserved but not yet broadcast (Dispatch only). Processing = already broadcast, awaiting a mined tx (Settle or Fail apply)."),
		title: t => t("tips.admin.withdrawals.state.title", "State") },
  "admin.withdrawals.settle.tx-hash": { type: "input", roles: OPS,
		body: t => t("tips.admin.withdrawals.settle.tx-hash.body", "Paste the mined on-chain tx hash. Settling records the mined transaction and releases the reservation — only enter a hash for a tx that actually mined."),
		title: t => t("tips.admin.withdrawals.settle.tx-hash.title", "Mined transaction hash") },
  "admin.withdrawals.fail.double-pay": { type: "section", roles: OPS,
		body: t => t("tips.admin.withdrawals.fail.double-pay.body", "Failing refunds the user. If the broadcast reached the chain this would double-pay — the hub refuses while a broadcast record exists, but verify on-chain first."),
		title: t => t("tips.admin.withdrawals.fail.double-pay.title", "Failing can double-pay") },
  "admin.withdrawals.destination": { type: "input", roles: OPS,
		body: t => t("tips.admin.withdrawals.destination.body", "The user's payout address, bound to this specific rail. Sends are irreversible and rail-specific — verify the address matches the network before dispatching."),
		title: t => t("tips.admin.withdrawals.destination.title", "Destination") },

  // ── admin · cabinet ─────────────────────────────────────────────────────────
  "admin.cabinet.flags": { type: "section", roles: OPS,
		body: t => t("tips.admin.cabinet.flags.body", "Flags gate cabinet features and MFE mounts. The row toggle flips a flag on or off; the rollout % controls what share of users it is exposed to."),
		title: t => t("tips.admin.cabinet.flags.title", "Feature flags") },
  "admin.cabinet.flags.rollout": { type: "input", roles: OPS,
		body: t => t("tips.admin.cabinet.flags.rollout.body", "The share of users the flag is enabled for during a staged rollout. 100% is a full rollout; lower values expose it to only that fraction."),
		title: t => t("tips.admin.cabinet.flags.rollout.title", "Rollout %") },
  "admin.cabinet.announcement.live": { type: "input", roles: OPS,
		body: t => t("tips.admin.cabinet.announcement.live.body", "Publishes the announcement banner across the whole cabinet immediately."),
		title: t => t("tips.admin.cabinet.announcement.live.title", "Live") },
  "admin.cabinet.maintenance": { type: "input", roles: OPS,
		body: t => t("tips.admin.cabinet.maintenance.body", "Swaps the whole cabinet for a holding page for all users — an identity-plane kill-switch. No money movement is affected."),
		title: t => t("tips.admin.cabinet.maintenance.title", "Maintenance mode") },
  "admin.cabinet.readonly": { type: "input", roles: OPS,
		body: t => t("tips.admin.cabinet.readonly.body", "Halts all deposit and withdrawal money movement — the money-plane kill-switch."),
		title: t => t("tips.admin.cabinet.readonly.title", "Read-only mode") },
} as const satisfies TipCatalog;

export type TipKey = keyof typeof tips;
