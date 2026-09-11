"use client";

import { useT } from "@evinvest/i18n/react";

import { Home, LineChart, ListChecks, Settings, Wallet, type LucideIcon } from "lucide-react";
import { motion, useReducedMotion } from "motion/react";
import { Link } from "@/shared/ui/cabinet-link";

import { prefetchOn } from "@/application/prefetch";
import { useCabinetPathname } from "@/shared/lib/cabinet-route";
import { cn } from "@/shared/lib/cn";
import { DUR, EASE } from "@/shared/ui/motion";

interface TabItem {
  href: `/${string}`;
  label: string;
  /** Catalogue key; `label` is what it resolves to in English. */
  key: string;
  icon: LucideIcon;
  active: (path: string) => boolean;
}

// The 5-tab mobile navigation bar (Figma cabinet mobile tab bar). These tabs
// replace the desktop sidebar on narrow viewports (<1024px).
//
// The predicates are pairwise disjoint, and have to stay that way: the active
// marker below is one element that moves to the matching tab, so two tabs
// answering true for the same path would make "the matching tab" ambiguous.
// The pair that used to overlap was Invest and a Products tab that pointed at
// /invest as well — Products is gone, and Wallet (a real destination with no
// tab of its own) took the slot. Operations no longer claims /wallet either,
// which was the other half of that collision.
const TABS: TabItem[] = [
  { href: "/", label: "Home", key: "nav.home", icon: Home, active: (p) => p === "/" },
  { href: "/invest", label: "Invest", key: "nav.invest", icon: LineChart, active: (p) => p.startsWith("/invest") },
  { href: "/operations", label: "Operations", key: "nav.operations", icon: ListChecks, active: (p) => p.startsWith("/operations") },
  { href: "/wallet", label: "Wallet", key: "nav.wallet", icon: Wallet, active: (p) => p.startsWith("/wallet") },
  { href: "/settings", label: "Settings", key: "nav.settings", icon: Settings, active: (p) => p.startsWith("/settings") },
];

/** Horizontal padding of the bar, as a length the marker's width math can use. */
const BAR_PX = "0.5rem";

export function BottomNavbar() {
  const t = useT();
  const pathname = useCabinetPathname();
  const reduce = useReducedMotion();
  const activeAt = TABS.findIndex((tab) => tab.active(pathname));
  const onTab = activeAt >= 0;

  return (
    <nav className="fixed bottom-0 left-0 right-0 z-50 flex h-[var(--cabinet-bottom-nav-h,64px)] items-center border-t border-border bg-secondary px-2 pb-[env(safe-area-inset-bottom,0px)] lg:hidden">
      {/* One marker for the whole bar, mounted once and translated — not a node
          per tab that mounts and unmounts.

          The old version rendered it inside the active <Link>, which had two
          consequences. It sat at the top of the *link box*, a couple of pixels
          off the icon rather than on the bar's edge. And on any route no tab
          claims — /profile, /notifications — the index was -1, so the marker
          unmounted completely and then reappeared out of nowhere on the way
          back, with no previous position to travel from. A shared `layoutId`
          cannot paper over that: an element that does not exist has no origin.

          Mounted permanently, both problems are arithmetic instead. Position is
          `index × 100%` of its own width, and its own width is exactly one tab,
          so the two can never drift. An unclaimed route just fades it out where
          it stands, and returning fades it back in already in the right place. */}
      <motion.span
        aria-hidden
        className="pointer-events-none absolute top-0 flex justify-center"
        style={{ left: BAR_PX, width: `calc((100% - 2 * ${BAR_PX}) / ${TABS.length})` }}
        initial={false}
        animate={{ x: `${Math.max(activeAt, 0) * 100}%`, opacity: onTab ? 1 : 0 }}
        transition={reduce ? { duration: 0 } : { duration: DUR.base, ease: EASE.out }}
      >
        <span className="h-0.5 w-10 rounded-full bg-accent-debug" />
      </motion.span>

      {TABS.map((tab) => {
        const Icon = tab.icon;
        const isActive = tab.active(pathname);
        return (
          <Link
            key={tab.label}
            href={tab.href}
            {...prefetchOn(tab.href)}
            aria-current={isActive ? "page" : undefined}
            className={cn(
              // `min-w-0` is load-bearing, not tidiness. A flex item's default
              // `min-width: auto` refuses to shrink below its content, so a long
              // label (Russian "Инвестировать", German "Einstellungen") made its
              // tab wider than the 1/5 share `flex-1` promises and shoved the
              // others off their marker positions — the marker's width is
              // computed as an exact fifth of the bar, so the two drift apart the
              // moment a tab is not that width. With `min-w-0` the tab can shrink
              // and the label truncates instead of the layout breaking.
              "flex min-w-0 flex-1 flex-col items-center justify-center gap-0.5 rounded-lg py-1 font-medium transition-colors",
              // The offset is what earns its keep here: the active tab's fill is the same
              // teal as the ring, so without a gap the ring reads as the pill growing.
              "outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-secondary",
              isActive ? "text-accent-debug" : "text-ink-soft hover:text-ink",
            )}
          >
            <Icon className="size-5 shrink-0" />
            {/* `truncate` is the net, not the plan. Five tabs on a 390px phone give each
                label a 75px box, measured; every `nav.*` value is authored to fit it.
                For the record, at 12px/500 Inter: "Operations" 63.1px, "Инвестиции"
                72.1px, "Paramètres" 65.6px — and the value this replaced, Russian
                "Инвестировать", 91.3px. That 16px is what broke the bar, because
                without `min-w-0` above the tab grew to fit rather than the label
                shrinking, and the marker is positioned as an exact fifth of the bar.
                German is the tightest case: "Einstellungen" measures 77.1px and does
                not fit, which is why `nav.settings` is "Optionen" there.
                The `title` is for assistive tech and for a desktop browser at a narrow
                width — not a touch affordance, since neither iOS nor Android surfaces
                one, and this bar is `lg:hidden`. The label is authored to fit; nothing
                here depends on the tooltip. */}
            <span className="w-full truncate text-center text-xs" title={t(tab.key)}>
              {t(tab.key)}
            </span>
          </Link>
        );
      })}
    </nav>
  );
}
