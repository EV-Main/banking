// The words of every tip in the catalog, beside the key they render under. A tip is prose
// a reader reads, so it goes through `t` like every other sentence; written here as literal
// `t(key, English)` pairs because the catalogue is extracted from the code. Each entry is a
// function so an anchor formats its own two strings, not all of them.

import type { Translate } from "@evinvest/i18n";

import type { TipKey } from "./catalog";

export interface TipCopy {
  title: string;
  body: string;
}

export const TIP_COPY: Readonly<Record<TipKey, (t: Translate) => TipCopy>> = {
  "wallet.balance.model": (t) => ({
    title: t("tips.wallet.balance.model.title", "One balance, many networks"),
    body: t("tips.wallet.balance.model.body", "You have a single USDT balance — networks are just how you deposit and withdraw. This card splits it into Total = Available + In orders + Invested + Pending withdrawal + In compartments."),
  }),
  "wallet.balance.available": (t) => ({
    title: t("tips.wallet.balance.available.title", "Available"),
    body: t("tips.wallet.balance.available.body", "Your spendable USDT right now — what you can invest into fund units or withdraw."),
  }),
  "wallet.balance.in-orders": (t) => ({
    title: t("tips.wallet.balance.in-orders.title", "In orders"),
    body: t("tips.wallet.balance.in-orders.body", "USDT set aside by your resting buy orders on the book. Still yours: it comes back to available when an order is cancelled, and turns into units when it fills."),
  }),
  "wallet.balance.invested": (t) => ({
    title: t("tips.wallet.balance.invested.title", "Invested"),
    body: t("tips.wallet.balance.invested.body", "Held in fund units, valued at the current NAV — units offered for sale on the book included."),
  }),
  "wallet.balance.pending-withdrawal": (t) => ({
    title: t("tips.wallet.balance.pending-withdrawal.title", "Pending withdrawal"),
    body: t("tips.wallet.balance.pending-withdrawal.body", "USDT reserved for withdrawals still in flight (queued or processing). It is no longer part of your available balance."),
  }),
  "wallet.balance.in-compartments": (t) => ({
    title: t("tips.wallet.balance.in-compartments.title", "In compartments"),
    body: t("tips.wallet.balance.in-compartments.body", "USDT you moved into a service's compartment. Still yours and counted in your total — move it back to your main balance to withdraw it."),
  }),
  "wallet.deposit.network": (t) => ({
    title: t("tips.wallet.deposit.network.title", "Network"),
    body: t("tips.wallet.deposit.network.body", "Pick the chain you'll send USDT on. Each network shows its own deposit address below — that address only works on that one chain."),
  }),
  "wallet.deposit.address": (t) => ({
    title: t("tips.wallet.deposit.address.title", "Your deposit address"),
    body: t("tips.wallet.deposit.address.body", "A personal, reusable address on this network. On TON it is shown in the wallet-friendly UQ… form so an uninitialised wallet doesn't bounce the transfer."),
  }),
  "wallet.deposit.min-confirmations": (t) => ({
    title: t("tips.wallet.deposit.min-confirmations.title", "Network confirmations"),
    body: t("tips.wallet.deposit.min-confirmations.body", "Credited to your one balance after the required number of network confirmations."),
  }),
  "wallet.deposit.rail-hazard": (t) => ({
    title: t("tips.wallet.deposit.rail-hazard.title", "Send only USDT on this network"),
    body: t("tips.wallet.deposit.rail-hazard.body", "Sending any other asset, or using a different network, loses the funds permanently. This 0x address is NOT your address on the sibling EVM chain — even though both start with 0x, USDT sent on the wrong EVM network will not be credited."),
  }),
  "wallet.withdraw.network": (t) => ({
    title: t("tips.wallet.withdraw.network.title", "Network"),
    body: t("tips.wallet.withdraw.network.body", "The chain your withdrawal ships on. The destination address must be a valid address on this exact network, or the funds are lost."),
  }),
  "wallet.withdraw.destination": (t) => ({
    title: t("tips.wallet.withdraw.destination.title", "Destination address"),
    body: t("tips.wallet.withdraw.destination.body", "The on-chain address the USDT is sent to. Withdrawals are irreversible — make sure it is correct and on the network you selected."),
  }),
  "wallet.withdraw.available": (t) => ({
    title: t("tips.wallet.withdraw.available.title", "Available to withdraw"),
    body: t("tips.wallet.withdraw.available.body", "The most you can withdraw on this network right now. Tap Max to fill the amount."),
  }),
  "wallet.withdraw.network-fee": (t) => ({
    title: t("tips.wallet.withdraw.network-fee.title", "Network fee"),
    body: t("tips.wallet.withdraw.network-fee.body", "A flat on-chain fee for this network, deducted from your amount before it is sent."),
  }),
  "wallet.withdraw.you-receive": (t) => ({
    title: t("tips.wallet.withdraw.you-receive.title", "You will receive"),
    body: t("tips.wallet.withdraw.you-receive.body", "Your amount minus the network fee — this is what actually lands at the destination."),
  }),
  "wallet.withdraw.queueing": (t) => ({
    title: t("tips.wallet.withdraw.queueing.title", "How withdrawals work"),
    body: t("tips.wallet.withdraw.queueing.body", "Up to your instant limit pays out immediately on this network; anything above it is queued until the network is topped up. A minimum applies per withdrawal."),
  }),
  "wallet.withdraw.review": (t) => ({
    title: t("tips.wallet.withdraw.review.title", "Review, then confirm"),
    body: t("tips.wallet.withdraw.review.body", "Review freezes exactly what you'll send. Confirm submits that snapshot — if the network list changes underneath, the open review is voided and you re-review."),
  }),
  "invest.overview": (t) => ({
    title: t("tips.invest.overview.title", "How investing works"),
    body: t("tips.invest.overview.body", "Subscribe USDT for units — your value tracks the fund's NAV, not the unit count."),
  }),
  "invest.position.units": (t) => ({
    title: t("tips.invest.position.units.title", "Units"),
    body: t("tips.invest.position.units.body", "The units you hold in this fund — a fixed quantity until you subscribe or redeem more."),
  }),
  "invest.position.nav": (t) => ({
    title: t("tips.invest.position.nav.title", "NAV"),
    body: t("tips.invest.position.nav.body", "Net asset value per unit — the current price at which units are bought on subscribe and priced on redeem."),
  }),
  "invest.position.value": (t) => ({
    title: t("tips.invest.position.value.title", "Value"),
    body: t("tips.invest.position.value.body", "Units × current NAV. It rises and falls as the NAV moves, not with the unit count."),
  }),
  "invest.position.stale-nav": (t) => ({
    title: t("tips.invest.position.stale-nav.title", "Stale NAV"),
    body: t("tips.invest.position.stale-nav.body", "The fund's price feed is out of date, so the value and P&L shown here may lag the true mark until the NAV refreshes."),
  }),
  "invest.position.pnl": (t) => ({
    title: t("tips.invest.position.pnl.title", "Profit & loss"),
    body: t("tips.invest.position.pnl.body", "Your position's current value minus what you paid in — green for a gain, red for a loss, driven by the NAV moving."),
  }),
  "invest.subscribe.amount": (t) => ({
    title: t("tips.invest.subscribe.amount.title", "Amount"),
    body: t("tips.invest.subscribe.amount.body", "How much USDT to subscribe. It is converted into units at the current NAV when you submit."),
  }),
  "invest.redeem.units": (t) => ({
    title: t("tips.invest.redeem.units.title", "Units to redeem"),
    body: t("tips.invest.redeem.units.body", "How many units to redeem — Max fills your full holding. The resulting cash isn't fixed here; it is set at the settle-time NAV."),
  }),
  "invest.redeem.queue": (t) => ({
    title: t("tips.invest.redeem.queue.title", "How redemptions settle"),
    body: t("tips.invest.redeem.queue.body", "Redemptions are accept-and-queue — your units are reserved now, and cash is priced at the settle-time NAV once the fund tops up."),
  }),
  "invest.activity.status": (t) => ({
    title: t("tips.invest.activity.status.title", "Redemption status"),
    body: t("tips.invest.activity.status.body", "Queued (reserved, awaiting settle), completed (paid out at the settle NAV), failed, or cancelled."),
  }),
  "invest.activity.cancel": (t) => ({
    title: t("tips.invest.activity.cancel.title", "Cancel"),
    body: t("tips.invest.activity.cancel.body", "Available only while a redemption is still queued. It withdraws the request and releases the reserved units back to your holding before any settle NAV is applied."),
  }),
  "trade.book": (t) => ({
    title: t("tips.trade.book.title", "How the book works"),
    body: t("tips.trade.book.body", "The book trades existing units between holders — a buyer and a seller meet at a bid or ask price they set themselves. Subscribing is different: it mints new units at the posted NAV, and redeeming returns them at NAV."),
  }),
  "dashboard.performance.portfolio-value": (t) => ({
    title: t("tips.dashboard.performance.portfolio-value.title", "Portfolio value"),
    body: t("tips.dashboard.performance.portfolio-value.body", "Your whole balance: available + invested + pending withdrawal. Money locked in a queued withdrawal is still counted here."),
  }),
  "dashboard.performance.all-time-return": (t) => ({
    title: t("tips.dashboard.performance.all-time-return.title", "All-time return"),
    body: t("tips.dashboard.performance.all-time-return.body", "Total unrealized P&L divided by what you put in at cost basis, across every position. A paper figure, not realized cash."),
  }),
  "dashboard.performance.series": (t) => ({
    title: t("tips.dashboard.performance.series.title", "Performance chart"),
    body: t("tips.dashboard.performance.series.body", "'Fund performance' is the fund's return since the first valuation in the range, from the posted marks; 'Your participation' is what your units were worth at each of them, which differs by when you contributed."),
  }),
  "dashboard.invested.allocation": (t) => ({
    title: t("tips.dashboard.invested.allocation.title", "Invested — what I own"),
    body: t("tips.dashboard.invested.allocation.body", "Each bar is one strategy's share of your invested value at current NAV. Percentages are of invested value only and exclude your available cash."),
  }),
  "dashboard.stats.unrealized-pnl": (t) => ({
    title: t("tips.dashboard.stats.unrealized-pnl.title", "Unrealized P&L"),
    body: t("tips.dashboard.stats.unrealized-pnl.body", "Paper gain or loss across all positions — current value minus cost basis. It becomes realized cash only when you redeem."),
  }),
  "dashboard.stats.available": (t) => ({
    title: t("tips.dashboard.stats.available.title", "Available"),
    body: t("tips.dashboard.stats.available.body", "Cash that is free and spendable now — ready to deploy into a strategy or withdraw."),
  }),
  "dashboard.stats.net-invested": (t) => ({
    title: t("tips.dashboard.stats.net-invested.title", "Net invested"),
    body: t("tips.dashboard.stats.net-invested.body", "Total cash you put into strategies at cost basis, net of redemptions — not the current value."),
  }),
  "settings.security.google-signin": (t) => ({
    title: t("tips.settings.security.google-signin.title", "Sign-in is managed by Google"),
    body: t("tips.settings.security.google-signin.body", "Your sign-in and password are managed by Google. Two-factor authentication and recovery are configured in your Google Account."),
  }),
  "settings.sessions.overview": (t) => ({
    title: t("tips.settings.sessions.overview.title", "Sessions & devices"),
    body: t("tips.settings.sessions.overview.body", "Where you're signed in — each row is a device or browser with a live session. Revoke anything you don't recognise."),
  }),
  "settings.sessions.this-device": (t) => ({
    title: t("tips.settings.sessions.this-device.title", "This device"),
    body: t("tips.settings.sessions.this-device.body", "The browser you're using right now. It stays signed in and can't be revoked from here — use 'Sign out all other devices' to clear the rest."),
  }),
  "settings.sessions.revoke": (t) => ({
    title: t("tips.settings.sessions.revoke.title", "Revoke"),
    body: t("tips.settings.sessions.revoke.body", "Signs that device out immediately. It must sign in again with Google to regain access."),
  }),
  "settings.sessions.revoke-others": (t) => ({
    title: t("tips.settings.sessions.revoke-others.title", "Sign out all other devices"),
    body: t("tips.settings.sessions.revoke-others.body", "Signs out every device except this one. Use it if a device was lost or you don't recognise a session."),
  }),
  "profile.personal.compliance": (t) => ({
    title: t("tips.profile.personal.compliance.title", "Why we collect this"),
    body: t("tips.profile.personal.compliance.body", "These personal details are collected to meet the fund's compliance obligations and to produce your account statements. They are not shown publicly."),
  }),
  "profile.field.legal-name": (t) => ({
    title: t("tips.profile.field.legal-name.title", "Legal name"),
    body: t("tips.profile.field.legal-name.body", "Your full name exactly as it appears on official documents — used for compliance and statements, distinct from your preferred name."),
  }),
  "profile.field.nationality": (t) => ({
    title: t("tips.profile.field.nationality.title", "Nationality"),
    body: t("tips.profile.field.nationality.body", "Collected for compliance and eligibility checks."),
  }),
  "profile.field.tax-residence": (t) => ({
    title: t("tips.profile.field.tax-residence.title", "Tax residence"),
    body: t("tips.profile.field.tax-residence.body", "The country where you're liable to pay tax, used for tax reporting. Usually where you live — it can differ from your nationality."),
  }),
  "profile.email.verified": (t) => ({
    title: t("tips.profile.email.verified.title", "Verified"),
    body: t("tips.profile.email.verified.body", "Confirms this email address has been verified. It is not identity or KYC verification."),
  }),
  "profile.kyc-level": (t) => ({
    title: t("tips.profile.kyc-level.title", "What your KYC level means"),
    body: t("tips.profile.kyc-level.body", "Level 0 is browse only: products, prices and your account. Level 1 means your identity is verified and opens a deposit address, withdrawals and subscribing."),
  }),
  "admin.users.access.role": (t) => ({
    title: t("tips.admin.users.access.role.title", "Role"),
    body: t("tips.admin.users.access.role.body", "The access level for this user: investor, operator or admin. Raising it grants operator/admin console access immediately on change. Owner is not here — a seat is the consilium's to give and to take away."),
  }),
  "admin.users.access.kyc-level": (t) => ({
    title: t("tips.admin.users.access.kyc-level.title", "KYC level"),
    body: t("tips.admin.users.access.kyc-level.body", "The user's identity-verification tier. Higher levels unlock higher limits and actions per the KYC policy."),
  }),
  "admin.users.access.revoke-sessions": (t) => ({
    title: t("tips.admin.users.access.revoke-sessions.title", "Revoke all sessions"),
    body: t("tips.admin.users.access.revoke-sessions.body", "Bumps token_version — invalidates every JWT issued to this user, signing them out of every active session."),
  }),
  "admin.users.identity.token-version": (t) => ({
    title: t("tips.admin.users.identity.token-version.title", "Token version"),
    body: t("tips.admin.users.identity.token-version.body", "A per-user counter stamped into every issued JWT. It increments on 'Revoke all sessions', so any token carrying an older version fails verification."),
  }),
  "admin.users.status.suspend": (t) => ({
    title: t("tips.admin.users.status.suspend.title", "Hold, suspension and reinstatement"),
    body: t("tips.admin.users.status.suspend.body", "Hold freezes the account now and lapses by itself in 24 hours — one operator may do that. Making it permanent, or undoing a permanent one, is a proposal the owners vote through. An account blocked before the two were told apart does not lapse, and one admin can lift it."),
  }),
  "admin.outbox.parked-events": (t) => ({
    title: t("tips.admin.outbox.parked-events.title", "Parked events"),
    body: t("tips.admin.outbox.parked-events.body", "A row parks when the relay hits a terminal apply error. Fix the cause shown in the Reason column first, then unpark to re-drive it — otherwise it just re-parks."),
  }),
  "admin.outbox.parked.reason": (t) => ({
    title: t("tips.admin.outbox.parked.reason.title", "Reason"),
    body: t("tips.admin.outbox.parked.reason.body", "The relay's failure cause for this parked row — the thing you must fix before unparking."),
  }),
  "admin.outbox.parked.compensated": (t) => ({
    title: t("tips.admin.outbox.parked.compensated.title", "Compensated"),
    body: t("tips.admin.outbox.parked.compensated.body", "This row's saga was already reversed, so its money effect is undone. Unpark is disabled to avoid re-applying an entry that was intentionally rolled back."),
  }),
  "admin.outbox.parked.unpark": (t) => ({
    title: t("tips.admin.outbox.parked.unpark.title", "Unpark"),
    body: t("tips.admin.outbox.parked.unpark.body", "Re-drives this outbox row through the relay after you've fixed its cause. Disabled when the row was compensated, already unparked this session, or while another unpark is in flight."),
  }),
  "admin.treasury.two-layer-model": (t) => ({
    title: t("tips.admin.treasury.two-layer-model.title", "Two layers"),
    body: t("tips.admin.treasury.two-layer-model.body", "Layer 1 is the ledger's network-agnostic USDT claims; Layer 2 is the actual on-chain liquidity held per rail. The two must reconcile."),
  }),
  "admin.treasury.layer1.ledger": (t) => ({
    title: t("tips.admin.treasury.layer1.ledger.title", "Layer 1 · Ledger"),
    body: t("tips.admin.treasury.layer1.ledger.body", "The ledger's network-agnostic claims in USDT. Total claims equal on-chain custody and break down into what users hold directly, what allocations hold for their holders, and reserved-for-withdrawals."),
  }),
  "admin.treasury.layer1.claims-total": (t) => ({
    title: t("tips.admin.treasury.layer1.claims-total.title", "Claims · total"),
    body: t("tips.admin.treasury.layer1.claims-total.body", "Total claims — users' and allocations'. Equals the sum of on-chain custody — every claim is fully backed."),
  }),
  "admin.treasury.layer1.held-by-users": (t) => ({
    title: t("tips.admin.treasury.layer1.held-by-users.title", "Held by users"),
    body: t("tips.admin.treasury.layer1.held-by-users.body", "Claims people hold directly — their wallet balances. Read off the user claim accounts, never derived as a remainder."),
  }),
  "admin.treasury.layer1.allocations": (t) => ({
    title: t("tips.admin.treasury.layer1.allocations.title", "Held through allocations"),
    body: t("tips.admin.treasury.layer1.allocations.body", "Every allocation's cash claim, supply, price and holders — the products and the hidden fee and fund allocations. Every unit belongs to a person or to the fee allocation; nothing is nobody's."),
  }),
  "admin.treasury.layer1.reserved-withdrawals": (t) => ({
    title: t("tips.admin.treasury.layer1.reserved-withdrawals.title", "Reserved · withdrawals"),
    body: t("tips.admin.treasury.layer1.reserved-withdrawals.body", "Claims set aside for queued and in-flight withdrawals, parked in the clearing account until they settle on-chain."),
  }),
  "admin.treasury.layer2.rails": (t) => ({
    title: t("tips.admin.treasury.layer2.rails.title", "Layer 2 · Treasury"),
    body: t("tips.admin.treasury.layer2.rails.body", "The actual on-chain USDT liquidity held per rail (BEP20 / TRC20 / TON / Polygon), plus the fiat bank balance. This is where the backing physically sits."),
  }),
  "admin.treasury.rail.funding": (t) => ({
    title: t("tips.admin.treasury.rail.funding.title", "Rail funding"),
    body: t("tips.admin.treasury.rail.funding.body", "The card value is this rail's ledger custody; the footer shows the hot wallet's real on-chain USDT and native gas. The two can legitimately diverge under accept-and-queue."),
  }),
  "admin.treasury.rail.address": (t) => ({
    title: t("tips.admin.treasury.rail.address.title", "Treasury address"),
    body: t("tips.admin.treasury.rail.address.body", "This rail's on-chain hot-wallet address. BEP20 and Polygon share an identical EVM address format — topping up on the wrong EVM chain, or sending another rail's funds here, is an irreversible mis-send."),
  }),
  "admin.treasury.rail.gas-station": (t) => ({
    title: t("tips.admin.treasury.rail.gas-station.title", "Gas station"),
    body: t("tips.admin.treasury.rail.gas-station.body", "Top this address up with the rail's native token to fund sweep gas. Send the correct native symbol on the correct chain — EVM gas-station addresses are shared, so a wrong-chain top-up is unrecoverable."),
  }),
  "admin.treasury.bank": (t) => ({
    title: t("tips.admin.treasury.bank.title", "Bank · USD"),
    body: t("tips.admin.treasury.bank.body", "The fiat USD bank balance used for off-ramp and FX, counted as treasury liquidity alongside the on-chain rails."),
  }),
  "admin.treasury.invariant": (t) => ({
    title: t("tips.admin.treasury.invariant.title", "The invariant"),
    body: t("tips.admin.treasury.invariant.body", "Per-rail backing is the treasury's job, not the ledger's: a shortfall on one rail is accept-and-queue, then rebalanced via CEX, alt-rail, or top-up. The global invariant is sum(custody) == sum(claims)."),
  }),
  "admin.valuation.post.aum": (t) => ({
    title: t("tips.admin.valuation.post.aum.title", "AUM (USDT)"),
    body: t("tips.admin.valuation.post.aum.body", "The fund's total assets under management you're marking. The NAV/share that pays redemptions is derived from it (AUM ÷ units outstanding), so a wrong figure mis-prices every settle."),
  }),
  "admin.valuation.post.derived-nav": (t) => ({
    title: t("tips.admin.valuation.post.derived-nav.title", "Derived NAV / share"),
    body: t("tips.admin.valuation.post.derived-nav.body", "Entered AUM divided by units outstanding. This is the mark that Post valuation commits."),
  }),
  "admin.valuation.post.nav-guard": (t) => ({
    title: t("tips.admin.valuation.post.nav-guard.title", "NAV-move guard"),
    body: t("tips.admin.valuation.post.nav-guard.body", "A post is refused if the NAV moves more than 50% from the last mark or from the mark of a week ago — stepping there in smaller posts is refused too. There is no override: a larger move is proposed to the owners and recorded only if their vote carries."),
  }),
  "admin.valuation.post.propose": (t) => ({
    title: t("tips.admin.valuation.post.propose.title", "Propose to the owners"),
    body: t("tips.admin.valuation.post.propose.body", "Opens a consilium instead of posting. The owners answer from their mailboxes; if more than half agree, the fund is marked at this AUM regardless of the NAV-move guard. Nothing changes until then."),
  }),
  "admin.valuation.queue.settle-fail": (t) => ({
    title: t("tips.admin.valuation.queue.settle-fail.title", "Settle vs Fail"),
    body: t("tips.admin.valuation.queue.settle-fail.body", "Settle pays at settle-time NAV once the fund claim is liquid; if the rail is short the payout queues until treasury tops up. Fail voids the request and refunds the units."),
  }),
  "admin.valuation.queue.est-cash": (t) => ({
    title: t("tips.admin.valuation.queue.est-cash.title", "Est. cash"),
    body: t("tips.admin.valuation.queue.est-cash.body", "A preview only — approximately units × current NAV. The actual payout settles at the settle-time NAV, not this figure."),
  }),
  "admin.valuation.queue.settle": (t) => ({
    title: t("tips.admin.valuation.queue.settle.title", "Settle"),
    body: t("tips.admin.valuation.queue.settle.body", "Pays at the settle-time NAV once the fund claim is liquid, queuing if the rail is short. Settle also burns the user's units."),
  }),
  "admin.valuation.queue.fail": (t) => ({
    title: t("tips.admin.valuation.queue.fail.title", "Fail"),
    body: t("tips.admin.valuation.queue.fail.body", "Voids the request and refunds the units — no cash is paid and the burned units are returned to the user."),
  }),
  "admin.withdrawals.flow": (t) => ({
    title: t("tips.admin.withdrawals.flow.title", "Dispatch, settle, fail"),
    body: t("tips.admin.withdrawals.flow.body", "Dispatch broadcasts a queued withdrawal once its rail has liquidity. Settle records the mined transaction and releases the reservation. Fail voids and refunds — only safe when nothing reached the chain."),
  }),
  "admin.withdrawals.gross-net": (t) => ({
    title: t("tips.admin.withdrawals.gross-net.title", "Gross / net"),
    body: t("tips.admin.withdrawals.gross-net.body", "Gross is the full amount debited from the user; net is what is actually sent on-chain after the withdrawal fee."),
  }),
  "admin.withdrawals.state": (t) => ({
    title: t("tips.admin.withdrawals.state.title", "State"),
    body: t("tips.admin.withdrawals.state.body", "Queued = accepted and reserved but not yet broadcast (Dispatch only). Processing = already broadcast, awaiting a mined tx (Settle or Fail apply)."),
  }),
  "admin.withdrawals.settle.tx-hash": (t) => ({
    title: t("tips.admin.withdrawals.settle.tx-hash.title", "Mined transaction hash"),
    body: t("tips.admin.withdrawals.settle.tx-hash.body", "Paste the mined on-chain tx hash. Settling records the mined transaction and releases the reservation — only enter a hash for a tx that actually mined."),
  }),
  "admin.withdrawals.fail.double-pay": (t) => ({
    title: t("tips.admin.withdrawals.fail.double-pay.title", "Failing can double-pay"),
    body: t("tips.admin.withdrawals.fail.double-pay.body", "Failing refunds the user. If the broadcast reached the chain this would double-pay — the hub refuses while a broadcast record exists, but verify on-chain first."),
  }),
  "admin.withdrawals.destination": (t) => ({
    title: t("tips.admin.withdrawals.destination.title", "Destination"),
    body: t("tips.admin.withdrawals.destination.body", "The user's payout address, bound to this specific rail. Sends are irreversible and rail-specific — verify the address matches the network before dispatching."),
  }),
  "admin.cabinet.flags": (t) => ({
    title: t("tips.admin.cabinet.flags.title", "Feature flags"),
    body: t("tips.admin.cabinet.flags.body", "Flags gate cabinet features and MFE mounts. The row toggle flips a flag on or off; the rollout % controls what share of users it is exposed to."),
  }),
  "admin.cabinet.flags.rollout": (t) => ({
    title: t("tips.admin.cabinet.flags.rollout.title", "Rollout %"),
    body: t("tips.admin.cabinet.flags.rollout.body", "The share of users the flag is enabled for during a staged rollout. 100% is a full rollout; lower values expose it to only that fraction."),
  }),
  "admin.cabinet.announcement.live": (t) => ({
    title: t("tips.admin.cabinet.announcement.live.title", "Live"),
    body: t("tips.admin.cabinet.announcement.live.body", "Publishes the announcement banner across the whole cabinet immediately."),
  }),
  "admin.cabinet.maintenance": (t) => ({
    title: t("tips.admin.cabinet.maintenance.title", "Maintenance mode"),
    body: t("tips.admin.cabinet.maintenance.body", "Swaps the whole cabinet for a holding page for all users — an identity-plane kill-switch. No money movement is affected."),
  }),
  "admin.cabinet.readonly": (t) => ({
    title: t("tips.admin.cabinet.readonly.title", "Read-only mode"),
    body: t("tips.admin.cabinet.readonly.body", "Halts all deposit and withdrawal money movement — the money-plane kill-switch."),
  }),
};
