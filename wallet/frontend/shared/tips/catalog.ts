// The single, central registry of UX tips: which anchors exist, how each one renders,
// and who is allowed to see it. The rendering engine (@evinvest/uikit InfoTip /
// SectionDescriptor) stays content-agnostic and never sees any copy until
// <TipAnchor> resolves a key.
//
// The COPY is not here. Each entry's heading and explanation live in `./copy.ts` as
// `t("tips.<key>.title", …)` / `t("tips.<key>.body", …)`, and <TipAnchor> resolves them
// with `useT()` — a tip is prose a reader reads, so it has to be translatable like every
// other sentence in the wallet host. What stays in this file is exactly what the copy is
// not: `type` picks the primitive and `roles` gates the render, and both are decided at
// compile time. English text is edited in `./copy.ts`; `copy.ts` is typed by `TipKey`,
// so a tip added here without its words fails `tsc`.
//
// Still authored as a typed `const` (not JSON) so every anchor id is a compile-time
// literal: a <TipAnchor anchor="…"> referencing a key that does not exist here fails
// `tsc`. That guarantee is the reason the key list did not move into the catalogue with
// the copy. Bodies are plain text (no markup) — the bubble is a non-interactive help
// hint, so there are no links to render.
//
// Keys are dot-namespaced by surface (`wallet.*`). An entry may gate itself to roles with
// `roles` (cosmetic only — server authz stays authoritative); no wallet tip needs it.
// Money-safety strings are lifted verbatim from the views so a tip can never drift from
// real behaviour — keep each pair in step by key.

export type TipType = "input" | "section";

interface TipEntryBase {
  /**
   * Optional platform-role gate. When set, only sessions whose role is listed
   * see the tip. Cosmetic only — server-side authorization stays authoritative.
   */
  roles?: readonly string[];
}

/** Which primitive renders this tip: an inline ⓘ toggletip, or a section block. */
export type TipEntry =
  | (TipEntryBase & { type: "input" })
  | (TipEntryBase & {
      type: "section";
      /**
       * Opens on demand rather than standing open. For a surface where the explanation
       * must not push the controls it explains out of view.
       */
      collapsible?: boolean;
    });

export type TipCatalog = Record<string, TipEntry>;

export const tips = {
  // ── wallet ──────────────────────────────────────────────────────────────────
  "wallet.balance.model": { type: "section" },
  "wallet.deposit.network": { type: "input" },
  "wallet.deposit.address": { type: "input" },
  "wallet.deposit.min-confirmations": { type: "input" },
  "wallet.deposit.rail-hazard": { type: "section" },
  "wallet.withdraw.network": { type: "input" },
  "wallet.withdraw.destination": { type: "input" },
  "wallet.withdraw.available": { type: "input" },
  "wallet.withdraw.network-fee": { type: "input" },
  "wallet.withdraw.you-receive": { type: "input" },
  "wallet.withdraw.queueing": { type: "section" },
  "wallet.withdraw.review": { type: "section" },
} as const satisfies TipCatalog;

export type TipKey = keyof typeof tips;
