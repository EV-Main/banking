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
    body: t("tips.wallet.balance.model.body", "You have a single USDT balance — networks are just how you deposit and withdraw. This card splits it into Total = Available + Pending withdrawal + In compartments."),
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
};
