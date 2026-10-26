// What a payment order's tier means for the money, in a sentence the investor reads before
// answering. Takes the caller's `t`, because this is a plain module with no translator of its
// own.
//
// The lookup is guarded by its table and falls back to the bare wire word: the tiers live
// in the money plane, and one it grows tomorrow should render as itself, not as
// `payment.tier.hint.<word>` (see `shared/contracts/payments.ts`).

import type { Translate } from "@evinvest/i18n";

import { wordFor } from "@/shared/lib/wire-words";

const tierHints = (t: Translate): Readonly<Record<string, string>> => ({
  internal: t("payment.tier.hint.internal", "Between two claims inside the fund. No money leaves the ledger."),
  service: t("payment.tier.hint.service", "Into one of the fund's products, as its pooled claim."),
  external: t("payment.tier.hint.external", "Out to an on-chain address. Ships as a withdrawal once approved."),
});

export function tierHint(tier: string, t: Translate): string {
  return wordFor(tierHints(t), tier) ?? tier;
}
