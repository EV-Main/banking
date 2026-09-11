"use client";

import { ChevronLeft } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import type { ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";

import { cn } from "@/shared/lib/cn";
import { Reveal } from "@/shared/ui/motion";

// The mobile app bar (Figma `cabinet/mobile/settings` node 498:260 and
// `cabinet/mobile/profile` node 503:267). Two shapes from one component: a root
// screen puts the title flush left with an optional trailing action, a pushed
// screen centres the title between a back affordance and a matching spacer.
// Hidden from `lg` up, where the desktop shell renders its own page heading.

// The back affordance is a bare icon with no border of its own, so it needs a rounded box
// for the focus ring to trace.
const BACK_FOCUS = "rounded-md outline-none focus-visible:ring-2 focus-visible:ring-ring";

export function MobileAppBar({
  title,
  backHref,
  onBack,
  right,
}: {
  title: string;
  backHref?: `/${string}`;
  onBack?: () => void;
  right?: ReactNode;
}) {
  const t = useT();
  const pushed = !!backHref || !!onBack;
  return (
    // Arrives with the screen it titles, one step ahead of the content column — the
    // bar is the first thing on a mobile page, and a title that snaps in above cards
    // that fade would be the loudest seam on the screen.
    //
    // The transform this animates is the element's own, which `position: sticky`
    // resolves after; it is an ancestor's transform that would break it, and this bar
    // has no positioned descendants to lose either.
    <Reveal as="header" className="sticky top-0 z-30 flex items-center gap-2 border-b border-border bg-secondary px-5 pb-3.5 pt-4 lg:hidden">
      {backHref ? (
        <Link href={backHref} aria-label={t("ui.back")} className={cn("-ml-1 flex size-6 shrink-0 items-center justify-center text-ink", BACK_FOCUS)}>
          <ChevronLeft className="size-6" />
        </Link>
      ) : (
        onBack && (
          <button type="button" onClick={onBack} aria-label={t("ui.back")} className={cn("-ml-1 flex size-6 shrink-0 items-center justify-center text-ink", BACK_FOCUS)}>
            <ChevronLeft className="size-6" />
          </button>
        )
      )}
      {/* Both sizes land on the type scale, so the pushed/root distinction is carried by
          size plus centring rather than the 3px that used to separate them.

          i18n-max: 20 — the title is caller-supplied, so the budget belongs to whichever
          key the caller passes. `truncate` means a long one degrades to an ellipsis
          rather than breaking the bar, but a pushed screen spends 24px on the back
          chevron and up to a Save button on the right, which leaves roughly 200px at
          text-base: about twenty characters before the ellipsis starts eating words. */}
      <h1 className={cn("min-w-0 flex-1 truncate font-semibold tracking-normal text-ink", pushed ? "text-center text-base" : "text-lg")}>{title}</h1>
      {pushed && !right ? <span className="size-6 shrink-0" aria-hidden /> : right}
    </Reveal>
  );
}
