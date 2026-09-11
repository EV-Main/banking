// Product marks for the allocation catalog — which glyph a fund is drawn with, and which
// tint frames it. Three surfaces draw one (the rail's Products group, the invest card, the
// product page header) and they must agree: the same fund has to look like itself
// everywhere, which is the whole reason the icon is an operator-chosen attribute rather
// than something each screen derives.
//
// Its own module with no `shared/ui/icons/index.ts` barrel, for the same reason
// `./networks.tsx` has none: a barrel would let a future icon set ride along into every
// route that only wanted this one.
//
// The mapping lives here rather than as a field on `Allocation` or on `Product`: a
// component reference on the wire object would pull all ten glyphs into every module that
// merely names a fund, and `views/invest/lib/product.ts` is deliberately pure with no React
// in it. `@/shared/contracts` stays a types-only module for the same reason.
//
// Bundle: the rail is a client component on every signed-in screen and lists every
// registered product, and any product may carry any of the ten icons — so there is no
// subset to split off, and the whole table legitimately belongs in the shared chunk.
// Loading it lazily would be actively wrong: `allocationsResource` is `persist: true`
// precisely so the rail paints its products on the first frame of a return visit, and a
// dynamically imported glyph would arrive a frame later and pop. What IS avoided is
// avoidable weight — named imports from `lucide-react` (which ships `sideEffects: false`
// ESM and sits in Next's default `optimizePackageImports`), so exactly ten glyph modules
// are pulled rather than the icon index.

import { ArrowLeftRight, Banknote, Building2, ChartCandlestick, ChartPie, Gem, Landmark, Percent, Rocket, Vault, type LucideIcon } from "lucide-react";

import type { AllocationIcon } from "@/shared/contracts/admin";

// `ChartCandlestick`/`ChartPie` rather than the `CandlestickChart`/`PieChart` spellings:
// both still resolve in 0.453.0, but only as deprecated aliases of these.
//
// Typed against the union, not `Record<string, …>`, so adding a value to `AllocationIcon`
// without choosing a glyph for it is a compile error rather than a blank square in the rail.
const PRODUCT_ICONS: Record<AllocationIcon, LucideIcon> = {
  fund: Landmark,
  real_estate: Building2,
  trading: ChartCandlestick,
  yield: Percent,
  venture: Rocket,
  treasury: Vault,
  commodity: Gem,
  credit: Banknote,
  index: ChartPie,
  arbitrage: ArrowLeftRight,
};

/** The catalog order — what the admin picker lists, and the order it lists it in.
 *  Mirrors `icon::ALL` in `contracts/src/allocation.rs`, `fund` first because it is the
 *  hub's default and therefore the pre-selected option. */
export const ALLOCATION_ICONS: readonly AllocationIcon[] = ["fund", "real_estate", "trading", "yield", "venture", "treasury", "commodity", "credit", "index", "arbitrage"];

// Widened to the wire's `string` once, here, so a value from a server newer than this build
// is an ordinary lookup miss instead of an `as` cast at every call site.
const BY_WIRE: Record<string, LucideIcon | undefined> = PRODUCT_ICONS;

/**
 * A product's mark, falling back to `fund` — which is also the hub's own default, so an
 * unrecognised value degrades to the picture a product would have had if none were chosen.
 *
 * Two distinct inputs land here and both must survive: `undefined`, because
 * `allocationsResource` mirrors the catalog into sessionStorage and a returning user's
 * first frame is an object serialised before this field existed; and an unknown string,
 * because a deployed client is routinely older than the server it talks to. Neither may
 * render an empty square.
 *
 * Sized by the caller — the badge framing it differs per surface. Decorative: every call
 * site renders the product's title in text beside it, so announcing it again would read
 * the fund twice.
 */
export function ProductIcon({ icon, className }: { icon: string | undefined; className?: string }) {
  // Indexed rather than returned from a `productIcon()` accessor: `react-hooks/static-components`
  // rejects a component bound from a call during render, since it cannot tell a table
  // lookup from a component built on the spot.
  const Icon = BY_WIRE[icon ?? ""] ?? PRODUCT_ICONS.fund;
  return <Icon className={className} aria-hidden focusable="false" />;
}

// Three accent tints, so a rail of funds does not read as one undifferentiated column.
// Decoration only — the glyph carries the identity, and the tint merely separates neighbours.
const PRODUCT_TONES = ["bg-accent-debug/15 text-accent-debug", "bg-positive/15 text-positive", "bg-accent-warn/15 text-accent-warn"];

/**
 * The tint framing a product's mark, keyed by the service id.
 *
 * Keyed by identity, never by the row's position: the rail used `PRODUCT_TONES[i % 3]`, so
 * registering one fund shifted every fund below it to a different colour — a change no
 * operator made and no investor could explain. Against the service id a product keeps its
 * colour for life, and the rail agrees with the invest card and the product page, which
 * sort their lists differently and would otherwise each pick a different tint for the same
 * fund.
 */
export function productTone(service: string): string {
  // FNV-ish rolling hash rather than the char sum: sums collide on anagrams, and
  // `alpha-fund`/`fund-alpha` are exactly the kind of pair an operator registers.
  let hash = 0;
  for (let i = 0; i < service.length; i += 1) hash = (hash * 31 + service.charCodeAt(i)) >>> 0;
  return PRODUCT_TONES[hash % PRODUCT_TONES.length];
}
