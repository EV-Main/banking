// Display helpers for wallet addresses and per-rail chrome. Amounts are formatted by the
// wallet host's one money module (`@/shared/lib/money`), which also owns the exact bigint
// math the withdraw preview runs on.

export { formatUsdt, fromBaseUnits, shortAddress, subUsdt, toBaseUnits, USDT_DECIMALS } from "@/shared/lib/money";

// Per-rail display chrome lives in `@/shared/lib/rail`: several slices render a network, and a
// `views/*` importing another `views/*` is a same-layer dependency.
export { isEvmRail, networkLabel, railMeta, type RailMeta } from "@/shared/lib/rail";
