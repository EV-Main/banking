"use client";

import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";

import { ArrowUpFromLine, Bell, Boxes, Gavel, Home, Landmark, LayoutGrid, LineChart, ListChecks, PanelsTopLeft, Percent, PiggyBank, Receipt, Settings, UsersRound, Wallet, type LucideIcon } from "lucide-react";
import { motion, useReducedMotion } from "motion/react";
import { Link } from "@/shared/ui/cabinet-link";
import { type ReactNode, useState } from "react";

import { prefetchOn } from "@/application/prefetch";
import { allocationsResource } from "@/entities/fund/model/fund-resource";
import { useUnreadCount, useUnreadCountPolling } from "@/entities/notification/model/notification-store";
import { useCabinetPathname } from "@/shared/lib/cabinet-route";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { useSession } from "@/shared/lib/use-session";
import { ProductIcon, productTone } from "@/shared/ui/icons/products";
import { DUR, EASE } from "@/shared/ui/motion";

interface NavItem {
  href: `/${string}`;
  /** The item's label, in the reader's language. */
  label: (t: Translate) => string;
  icon: LucideIcon;
  active: (path: string) => boolean;
}

// FUND group — the primary surfaces. "Home" is the portfolio dashboard; "Wallet" is the
// balance plus deposit/withdraw/activity, which the dashboard's money actions also link
// into. The mobile tab bar has no room for a sixth tab, so there /wallet still lights up
// Operations.
const FUND: NavItem[] = [
  { href: "/", label: t => t("nav.home", "Home"), icon: Home, active: (p) => p === "/" },
  // Exactly `/invest`, not everything beneath it: each product has its own row in
  // PRODUCTS, and a prefix match here lit both that row and this one on a product
  // page. Two highlighted rows is not a state the rail should be able to reach —
  // and with the pill scoped per section it would mean two pills at once.
  { href: "/invest", label: t => t("nav.invest", "Invest"), icon: LineChart, active: (p) => p === "/invest" },
  { href: "/wallet", label: t => t("nav.wallet", "Wallet"), icon: Wallet, active: (p) => p.startsWith("/wallet") },
  { href: "/operations", label: t => t("nav.operations", "Operations"), icon: ListChecks, active: (p) => p.startsWith("/operations") },
];

// PRODUCTS is the open allocation registry, not a fixed list: a fund appears in the rail
// because an operator registered and opened it. It used to name one product literally,
// which went stale the moment a second one was registered.
//
// The mark and its tint both come from `@/shared/ui/icons/products`, so the rail, the
// invest card and the product page draw the same fund the same way.

// ADMINISTER group — the operator console. Rendered only for a non-investor session
// role (the BFF's `/api/auth/session` `isAdmin`); every screen is also authorized
// server-side, so hiding the nav is cosmetic, not the security boundary.
const ADMIN: NavItem[] = [
  { href: "/admin/overview", label: t => t("nav.overview", "Overview"), icon: LayoutGrid, active: (p) => p.startsWith("/admin/overview") },
  { href: "/admin/users", label: t => t("nav.users", "Users"), icon: UsersRound, active: (p) => p.startsWith("/admin/users") },
  { href: "/admin/cabinet", label: t => t("nav.cabinet", "Cabinet"), icon: PanelsTopLeft, active: (p) => p.startsWith("/admin/cabinet") },
  { href: "/admin/treasury", label: t => t("nav.treasury", "Treasury"), icon: Landmark, active: (p) => p.startsWith("/admin/treasury") },
  { href: "/admin/withdrawals", label: t => t("nav.withdrawals", "Withdrawals"), icon: ArrowUpFromLine, active: (p) => p.startsWith("/admin/withdrawals") },
  // Sits next to Treasury, not to Withdrawals: the question it answers is "what did the
  // fund earn", which belongs with the chart of accounts rather than with the user queue.
  { href: "/admin/revenue", label: t => t("nav.revenue", "Fund revenue"), icon: PiggyBank, active: (p) => p.startsWith("/admin/revenue") },
  // Directly under Fund revenue, because the consilium is what authorizes money leaving it
  // — the page and the balance it governs read as one thought. Shown to every operator
  // session like its neighbours; only owners have a room to be in, and the BFF answers 403
  // to anyone else (the page renders that as its own state, not as an error).
  { href: "/consilium", label: t => t("nav.consilium", "Consilium"), icon: Gavel, active: (p) => p.startsWith("/consilium") },
  { href: "/admin/allocations", label: t => t("nav.allocations", "Allocations"), icon: Boxes, active: (p) => p.startsWith("/admin/allocations") },
  { href: "/admin/valuation", label: t => t("nav.valuation", "Valuation & redemptions"), icon: Receipt, active: (p) => p.startsWith("/admin/valuation") },
  // After Allocations, because a fee is a property OF a product: you register the fund
  // first and then price it. Distinct from Fund revenue, which is where the money ends up
  // once these terms have been charged and settled.
  { href: "/admin/fees", label: t => t("nav.fees", "Fees"), icon: Percent, active: (p) => p.startsWith("/admin/fees") },
];

// Every rail row is a hand-written Link, so keyboard focus rides on this string. It runs
// The ring is the solid token, never a tint: at 50% the teal composites to 1.9–2.3:1 against
// every surface in this theme, under the 3:1 SC 1.4.11 floor, where solid clears it
// everywhere. The offset keeps the ring legible around the active row, whose fill is that
// same teal.
const NAV_FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-secondary";

// The signed-in app shell's left rail (Figma cabinet sidebar). Persistent across the
// `(app)` route group; auth is enforced upstream in `proxy.ts`. Positioned by a
// `lg:fixed` wrapper in the `(app)` layout — below 1024px the sidebar is hidden,
// replaced by the fixed BottomNavbar. `overflow-y-auto` is a safety valve only: the
// rail scrolls internally solely when it can't fit (e.g. the admin nav on a short
// viewport), so nothing gets clipped.
export function Sidebar() {
  // Zone-relative, so the `active` predicates below can stay written in the paths
  // the app reasons in rather than the ones the browser shows.
  const t = useT();
  const pathname = useCabinetPathname();
  const session = useSession();
  const isAdmin = session?.user?.isAdmin ?? false;
  // The rail is mounted on every signed-in screen, so it is the one place the unread
  // count is polled from — every other consumer reads the shared store.
  useUnreadCountPolling();
  const unread = useUnreadCount();
  // The catalog is cached and mirrored to sessionStorage, so the rail lists its products on
  // the first frame of a return visit rather than growing a Products group a beat later. A
  // failed read leaves the group empty rather than blocking the rail — the nav is not the
  // place to surface an API error.
  const products = useResource(allocationsResource).data?.allocations ?? [];
  // Which section owns the current route, and whether getting here crossed a
  // boundary. A product page belongs to no pill-owning section, so it is null and
  // the next move into one counts as a crossing.
  const activeSection: Section | null = products.some(
    (p) => pathname === `/invest/${encodeURIComponent(p.service)}`,
  )
    ? "products"
    : FUND.some((i) => i.active(pathname))
      ? "fund"
      : isAdmin && ADMIN.some((i) => i.active(pathname))
        ? "administer"
        : pathname.startsWith("/notifications") || pathname.startsWith("/settings")
          ? "secondary"
          : null;
  const crossed = useCrossedSection(pathname, activeSection);

  return (
    <aside className="flex h-full w-[var(--cabinet-rail-w)] flex-col gap-7 overflow-y-auto border-r border-border bg-secondary px-4.5 pb-5 pt-6">
      <nav aria-label={t("nav.a11y.primary", "Primary")} className="flex flex-col gap-4.5">
        <Group label={t("nav.group.fund", "Fund")}>
          {FUND.map((item) => (
            <NavLink key={item.href} item={item} active={item.active(pathname)} section="fund" appear={crossed} />
          ))}
        </Group>
        {products.length > 0 && (
          <Group label={t("invest.products", "Products")}>
            {products.map((p) => {
              // Every row pointed at `/invest`, so naming a product in the rail took you to
              // the list of all of them. The product's own page is keyed by its service id.
              const href: `/${string}` = `/invest/${encodeURIComponent(p.service)}`;
              const active = pathname === href;
              return (
                <Link
                  key={p.service}
                  href={href}
                  {...prefetchOn(href)}
                  aria-current={active ? "page" : undefined}
                  className={cn(
                    // `isolate` for the same reason NavLink needs it: the pill sits
                    // on a negative z-index and would otherwise land behind the
                    // rail's own background rather than behind the label.
                    "relative isolate flex items-center gap-3 rounded-lg px-3 py-2 text-sm transition-colors",
                    NAV_FOCUS,
                    active ? "font-semibold text-primary-foreground" : "font-medium text-ink hover:bg-ink/5",
                  )}
                >
                  {active && <ActivePill section="products" appear={crossed} />}
                  <span className={cn("flex size-5 shrink-0 items-center justify-center rounded-md", productTone(p.service))}>
                    <ProductIcon icon={p.icon} className="size-3.5" />
                  </span>
                  <span className="truncate">{p.title}</span>
                </Link>
              );
            })}
          </Group>
        )}
        {isAdmin && (
          <Group label={t("admin.eyebrow.administer", "Administer")}>
            {ADMIN.map((item) => (
              <NavLink key={item.href} item={item} active={item.active(pathname)} section="administer" appear={crossed} />
            ))}
          </Group>
        )}
      </nav>

      <div className="flex-1" />

      <nav aria-label={t("nav.a11y.secondary", "Secondary")} className="flex flex-col gap-1">
        <NavLink
          item={{ href: "/notifications", label: t => t("nav.notifications", "Notifications"), icon: Bell, active: (p) => p.startsWith("/notifications") }}
          active={pathname.startsWith("/notifications")}
          section="secondary"
          appear={crossed}
          trailing={unread ? <UnreadPill count={unread} active={pathname.startsWith("/notifications")} /> : undefined}
        />
        <NavLink item={{ href: "/settings", label: t => t("nav.settings", "Settings"), icon: Settings, active: (p) => p.startsWith("/settings") }} active={pathname.startsWith("/settings")} section="secondary" appear={crossed} />
      </nav>
    </aside>
  );
}

function Group({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-1 pl-1">
      <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{label}</p>
      {children}
    </div>
  );
}

// The active pill is one shared node per SECTION, not one for the whole rail and
// not a background class on each link.
//
// Sharing a `layoutId` is what makes motion track the pill from the old link to
// the new one and slide it there, so a move inside a section reads as a move. The
// id is scoped to the section because that same behaviour is wrong across
// sections: jumping from Invest to Users sent the pill travelling the length of
// the rail, straight through two headings that have nothing to do with either
// row. The distance implied a relationship between them that does not exist.
//
// With the id scoped, motion finds no counterpart in the section being entered,
// so the pill mounts fresh there — and `initial`/`animate` below turn that into a
// plain fade. Sliding within a section, appearing between them, from one lever.
const ACTIVE_PILL = "cabinet-rail-active";

/** Rail sections that own a pill. Every group that can hold the active row has one. */
type Section = "fund" | "products" | "administer" | "secondary";

/**
 * Whether the pill should fade in on mount — true only when the last navigation
 * crossed a section boundary.
 *
 * It has to be derived rather than left to motion: `initial` is honoured whenever
 * the pill mounts without a layout counterpart, but motion cannot tell us that
 * happened, and setting `initial` unconditionally makes the pill fade *while it
 * slides* inside a section too. Measured: with a blanket `initial` a within-
 * section move ran 13 positions AND 12 opacity steps; the slide alone is what was
 * asked for.
 *
 * Keyed on the path, not the section: a move inside a section leaves the section
 * unchanged, so comparing sections alone would keep whatever the previous answer
 * was and fade anyway. Adjusting state during render (rather than in an effect)
 * is what lets the answer be correct on the very render the pill mounts on.
 */
function useCrossedSection(pathname: string, section: Section | null): boolean {
  const [prev, setPrev] = useState({ path: pathname, section, crossed: false });
  if (prev.path !== pathname) {
    setPrev({ path: pathname, section, crossed: prev.section !== section });
    return prev.section !== section;
  }
  return prev.crossed;
}

// The highlight itself, shared by both row shapes in the rail — the nav rows and
// the product rows. It lives in one place because the two used to style
// the active state differently: NavLink drew this pill, PRODUCTS set `bg-primary`
// on the row, and so moving into or out of a product was the one transition in the
// rail that did not animate at all.
function ActivePill({ section, appear }: { section: Section; appear: boolean }) {
  const reduce = useReducedMotion();
  return (
    <motion.span
      // Dropping the id under reduced motion turns the slide into a cut while
      // leaving the highlight itself in place.
      layoutId={reduce ? undefined : `${ACTIVE_PILL}-${section}`}
      aria-hidden
      className="absolute inset-0 -z-10 rounded-lg bg-primary"
      // `false` disables the mount animation outright, which is what a move inside
      // a section wants: the pill slides at full opacity and must not also fade
      // while doing it.
      initial={appear ? { opacity: 0 } : false}
      animate={{ opacity: 1 }}
      transition={{ duration: DUR.base, ease: EASE.out }}
    />
  );
}

function NavLink({ item, active, section, appear, trailing }: { item: NavItem; active: boolean; section: Section; appear: boolean; trailing?: ReactNode }) {
  const t = useT();
  const Icon = item.icon;
  const label = item.label(t);
  return (
    <Link
      href={item.href}
      {...prefetchOn(item.href)}
      aria-current={active ? "page" : undefined}
      className={cn(
        // `isolate` is load-bearing: it gives the link its own stacking context so
        // the pill's negative z-index stays behind the label and not behind the
        // rail's own background, which is where it would land otherwise.
        "relative isolate flex items-center gap-3 rounded-lg px-3 py-2 text-sm transition-colors",
        NAV_FOCUS,
        active ? "font-semibold text-primary-foreground" : "font-medium text-ink hover:bg-ink/5",
      )}
    >
      {active && <ActivePill section={section} appear={appear} />}
      <Icon className="size-4.5 shrink-0" />
      {/* `min-w-0` for the same reason as the mobile tab bar: without it the
          label refuses to shrink below its own text, and the rail widens to fit
          the longest translation instead of the label truncating inside it —
          German "Benachrichtigungen" and "Bewertung & Rücknahmen" are both wider
          than the rail. `title` keeps the full label reachable. */}
      <span className="min-w-0 flex-1 truncate" title={label}>
        {label}
      </span>
      {trailing}
    </Link>
  );
}

// Capped at 99+ so a long-neglected inbox cannot widen the rail.
//
// The label and the text deliberately disagree past 99: the pill shows "99+" because the
// rail has no room for more, while the accessible name carries the real number, which is
// the one piece of information the cap throws away. The label was `${count} unread` — an
// English plural assembled by concatenation, hidden where nothing renders it, so every
// locale announced it in English.
function UnreadPill({ count, active }: { count: number; active: boolean }) {
  const t = useT();
  return (
    <span
      aria-label={t("notif.unreadCount", "Unread · {n}", { n: count })}
      className={cn(
        "rounded-full px-2 py-0.5 text-xs font-semibold tabular-nums",
        active ? "bg-background text-ink" : "bg-accent-debug/15 text-accent-debug",
      )}
    >
      {count > 99 ? "99+" : count}
    </span>
  );
}
