// Display helpers for wallet addresses and per-rail chrome. Amounts are formatted by the
// cabinet's one money module (`@/shared/lib/money`), which also owns the exact bigint
// math the withdraw preview runs on.

export { formatUsdt, fromBaseUnits, shortAddress, subUsdt, toBaseUnits, USDT_DECIMALS } from "@/shared/lib/money";

// Per-rail display chrome moved to `@/shared/lib/rail` when the governance surfaces began
// rendering a network too: a rail's display name is shared by three slices now, and a
// `views/*` importing another `views/*` is a same-layer dependency. Re-exported here so the
// wallet's own call sites keep reading from one place.
export { isEvmRail, networkLabel, railMeta, type RailMeta } from "@/shared/lib/rail";
