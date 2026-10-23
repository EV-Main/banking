// The EV Investment lockup — the brand mark over the wordmark, as one CSS-masked span.
//
// Local since `@evinvest/uikit` 0.10: EV-invest/lib#105 (1a48dc2) cut `Logo` from the kit
// together with the other EV-only chrome, because a kit every non-EV consumer would have
// overridden class by class is not a kit. 0.15 (056761a) brought the mechanism back and
// left the artwork with the consumer: the kit's `Logo` masks `--brand-mark` /
// `--brand-aspect`, declared beside the palette. This span masks the same two tokens, so
// the artwork lives once — in `application/styles/globals.css` — and the login lockup and
// the 404 page's mark (uikit `StatusScreen`) cannot drift apart. It stays a local component
// for its `role="img"` label: the kit's is `aria-hidden` on the argument that a brand name
// is always adjacent in text, which does not hold on the login and logged-out screens.
//
// Monochrome — paints with the current text color, so callers tint it via `text-*` (mist
// on dark surfaces) and size it by height alone (`h-8 w-auto`). A masked <span> has no
// intrinsic size the way an inline <svg viewBox> does, so `--brand-aspect` is what `w-auto`
// resolves against.

import type { CSSProperties } from "react";

import { cn } from "@/shared/lib/cn";

const MASK_STYLE: CSSProperties = {
  backgroundColor: "currentColor",
  maskImage: "var(--brand-mark)",
  WebkitMaskImage: "var(--brand-mark)",
  maskRepeat: "no-repeat",
  WebkitMaskRepeat: "no-repeat",
  maskPosition: "center",
  WebkitMaskPosition: "center",
  maskSize: "contain",
  WebkitMaskSize: "contain",
  aspectRatio: "var(--brand-aspect, 1)",
};

export function Logo({ className }: { className?: string }) {
  return <span data-slot="logo" role="img" aria-label="EV Investment" style={MASK_STYLE} className={cn("inline-block", className)} />;
}
