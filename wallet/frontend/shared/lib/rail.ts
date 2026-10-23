// Per-rail display chrome: the chain's own name, the badge glyph, and the accent tier its
// badge is tinted with.
//
// This moved out of `views/wallet/lib/format.ts` when the governance surfaces arrived. It
// had been a wallet detail for as long as only the wallet named a chain; now the payout
// approval page and the owners' room both render a network beside an address, and a rail's
// display name is not the wallet slice's property to lend them — a `views/*` importing
// another `views/*` is a same-layer dependency, and the layer below is where a fact shared
// by three slices belongs. `views/wallet/lib/format.ts` re-exports these, so every existing
// call site is unchanged.
//
// `label` and `badge` are the rail's own marks — `BEP20`, `TON`, `◆` — and stay literal in
// every locale. The chain's *name* is prose, so it travels as a function of `t` resolved at
// the render site: this module is plain TypeScript and has no translator of its own.
//
// The chain LOGOS deliberately do not live here, and adding an `icon` field would be a
// regression: `@/shared/ui/icons/networks` owns them, keyed by the same wire id. Two
// surfaces (payout approval, consilium) import this module for `networkLabel` alone, and a
// component reference on `RailMeta` would pull every logo path into their route bundles for
// a mark they never draw — besides making this data module depend on React. `badge` stays
// as the fallback a rail with no logo renders instead.

import type { Translate } from "@evinvest/i18n";

export interface RailMeta {
  label: string;
  /** The chain's name in the reader's language. */
  chain: (t: Translate) => string;
  badge: string;
  tone: string;
}

const RAILS: Readonly<Record<string, RailMeta>> = {
  bep20: { label: "BEP20", chain: (t) => t("wallet.chain.bep20", "BNB Smart Chain"), badge: "B", tone: "bg-chart-3/15 text-chart-3" },
  trc20: { label: "TRC20", chain: (t) => t("wallet.chain.trc20", "TRON"), badge: "T", tone: "bg-chart-4/15 text-chart-4" },
  ton: { label: "TON", chain: (t) => t("wallet.chain.ton", "The Open Network"), badge: "◆", tone: "bg-chart-1/15 text-chart-1" },
  polygon: { label: "Polygon", chain: (t) => t("wallet.chain.polygon", "Polygon PoS"), badge: "P", tone: "bg-chart-1/15 text-chart-1" },
};

export function railMeta(network: string | undefined): RailMeta {
  const id = network ?? "";
  return (Object.hasOwn(RAILS, id) ? RAILS[id] : undefined) ?? { label: id.toUpperCase(), chain: (t) => t("wallet.chain.unknown", "Network"), badge: (id[0] ?? "?").toUpperCase(), tone: "bg-muted text-ink-soft" };
}

export function networkLabel(network: string | undefined): string {
  return railMeta(network).label;
}

// The EVM rails share the exact same `0x…` address format, so one rail's deposit address is a
// syntactically valid — but never credited — destination on the other. That collision is unique
// to the EVM rails (TON/TRON addresses look nothing alike), so the deposit view calls it out by
// name only for them.
const EVM_RAILS = new Set(["bep20", "polygon"]);

export function isEvmRail(network: string | undefined): boolean {
  return EVM_RAILS.has(network ?? "");
}
