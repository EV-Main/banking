// Blockchain network marks, as inline JSX.
//
// Inline rather than an asset import: SVGR is not configured in this app, so
// `import Bnb from "./bnb.svg"` does not resolve. The uikit exports no icon primitive
// either — `Avatar`/`EmptyMedia` are containers, not marks — so the precedent this
// follows is the inline Google logo in `features/auth/ui/provider-marks.tsx`.
//
// This is deliberately its own module and gets no `shared/ui/icons/index.ts` barrel: the
// paths below are ~4KB of markup that only the surfaces naming a chain should carry, and
// a barrel would let any future icon set drag them along.
//
// For the same reason the mapping lives here rather than as a field on `RailMeta` in
// `@/shared/lib/rail`: the payout-approval and consilium views import that module for
// `networkLabel` alone, and a component reference on the meta object would pull every path
// below into their route bundles for a mark they never draw. `@/shared/lib/rail` also
// stays a plain data module with no React in it.
//
// Paths: BNB Chain, TON and Polygon from simple-icons (CC0-1.0); TRON from
// spothq/cryptocurrency-icons (CC0-1.0). Each is normalised to a 24×24 box and one path.
//
// `fill="currentColor"`, not the brand hex: a mark sits inside a badge that is already
// tinted from the accent tokens, and a hardcoded colour is the thing the design rules
// exist to keep off these surfaces. The chain is still identified by its silhouette.

import type { ComponentType, SVGProps } from "react";

import { railMeta } from "@/shared/lib/rail";

type MarkProps = SVGProps<SVGSVGElement>;

export type NetworkIcon = ComponentType<MarkProps>;

// Decorative by default — every call site renders the rail's name beside the mark, so
// announcing it again would read the chain twice.
function Mark({ children, ...props }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden focusable="false" {...props}>
      {children}
    </svg>
  );
}

export function BnbIcon(props: MarkProps) {
  return (
    <Mark {...props}>
      <path d="M5.631 3.676 12.001 0l6.367 3.676-2.34 1.358L12 2.716 7.972 5.034l-2.34-1.358Zm12.737 4.636-2.34-1.358L12 9.272 7.972 6.954l-2.34 1.358v2.716l4.026 2.318v4.636L12 19.341l2.341-1.359v-4.636l4.027-2.318V8.312Zm0 7.352v-2.716l-2.34 1.358v2.716l2.34-1.358Zm1.663.96-4.027 2.318v2.717l6.368-3.677V10.63l-2.34 1.358v4.636Zm-2.34-10.63 2.34 1.358v2.716l2.341-1.358V5.994l-2.34-1.358-2.342 1.358ZM9.657 19.926v2.716L12 24l2.341-1.358v-2.716l-2.34 1.358-2.343-1.358Zm-4.027-4.262 2.341 1.358v-2.716l-2.34-1.358v2.716Zm4.027-9.67L12 7.352l2.341-1.358-2.34-1.358-2.343 1.358Zm-5.69 1.358L6.31 5.994 3.968 4.636l-2.34 1.358V8.71l2.34 1.358V7.352Zm0 4.636-2.34-1.358v7.352l6.368 3.677v-2.717l-4.028-2.318v-4.636Z" />
    </Mark>
  );
}

export function TronIcon(props: MarkProps) {
  return (
    <Mark {...props}>
      {/* Authored on a 32×32 grid upstream; scaled rather than re-plotted so the curve
          data stays byte-identical to the CC0 source. */}
      <path
        transform="scale(0.75)"
        d="M21.932 9.913L7.5 7.257l7.595 19.112 10.583-12.894-3.746-3.562zm-.232 1.17l2.208 2.099-6.038 1.093 3.83-3.192zm-5.142 2.973l-6.364-5.278 10.402 1.914-4.038 3.364zm-.453.934l-1.038 8.58L9.472 9.487l6.633 5.502zm.96.455l6.687-1.21-7.67 9.343.983-8.133z"
      />
    </Mark>
  );
}

export function TonIcon(props: MarkProps) {
  return (
    <Mark {...props}>
      <path d="M12 0C5.373 0 0 5.373 0 12s5.373 12 12 12 12-5.373 12-12S18.627 0 12 0zM7.902 6.697h8.196c1.505 0 2.462 1.628 1.705 2.94l-5.059 8.765a.86.86 0 0 1-1.488 0L6.199 9.637c-.758-1.314.197-2.94 1.703-2.94zm4.844 1.496v7.58l1.102-2.128 2.656-4.756a.465.465 0 0 0-.408-.696h-3.35zM7.9 8.195a.464.464 0 0 0-.408.694l2.658 4.754 1.102 2.13V8.195H7.9z" />
    </Mark>
  );
}

export function PolygonIcon(props: MarkProps) {
  return (
    <Mark {...props}>
      <path d="m17.82 16.342 5.692-3.287A.98.98 0 0 0 24 12.21V5.635a.98.98 0 0 0-.488-.846l-5.693-3.286a.98.98 0 0 0-.977 0L11.15 4.789a.98.98 0 0 0-.489.846v11.747L6.67 19.686l-3.992-2.304v-4.61l3.992-2.304 2.633 1.52V8.896L7.158 7.658a.98.98 0 0 0-.977 0L.488 10.945a.98.98 0 0 0-.488.846v6.573a.98.98 0 0 0 .488.847l5.693 3.286a.981.981 0 0 0 .977 0l5.692-3.286a.98.98 0 0 0 .489-.846V6.618l.072-.041 3.92-2.263 3.99 2.305v4.609l-3.99 2.304-2.63-1.517v3.092l2.14 1.236a.981.981 0 0 0 .978 0v-.001Z" />
    </Mark>
  );
}

// Keyed by the hub's wire id, the same key `railMeta` reads. A rail with no mark here is
// not an error: it falls through to the letter badge below, which is what lets a rail the
// hub adds later render correctly with no code change at all.
const NETWORK_ICONS: Record<string, NetworkIcon> = {
  bep20: BnbIcon,
  trc20: TronIcon,
  ton: TonIcon,
  polygon: PolygonIcon,
};

/** The rail's mark, or — for a chain this build has no logo for — the letter badge
 *  `railMeta` already computes. One place owns that fallback so no call site has to
 *  restate it, and every surface degrades the same way on the same unknown rail.
 *
 *  Sized by the caller (`size-4`, `size-5`): the badge that frames it differs per surface.
 *  Inside a uikit `Badge` the className can be omitted — it sizes its own svg child. */
export function NetworkMark({ network, className }: { network: string | undefined; className?: string }) {
  // Indexed here rather than behind a `networkIcon()` accessor: `react-hooks/static-components`
  // rejects a component bound from a call during render, since it cannot tell a table
  // lookup from a component built on the spot.
  const Icon = NETWORK_ICONS[network ?? ""];
  // The letter is `aria-hidden` for the same reason the svg is: every call site renders
  // the rail's name in text beside it. Left readable it would join the accessible name of
  // whatever contains it — the segmented picker's radios take their name from their own
  // text content, and would announce an unknown rail as "S SUI".
  return Icon ? <Icon className={className} /> : <span aria-hidden>{railMeta(network).badge}</span>;
}
