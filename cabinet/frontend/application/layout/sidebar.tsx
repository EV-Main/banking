"use client";

import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";

import { ArrowLeftRight, ArrowUpFromLine, Bell, Boxes, Coins, Gavel, Home, Inbox, Landmark, LifeBuoy, LineChart, ListChecks, PanelsTopLeft, Percent, PiggyBank, Receipt, Settings, UserRound, UsersRound, Wallet, type LucideIcon } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { type MouseEvent, type ReactNode, useLayoutEffect, useRef, useState } from "react";

import { prefetchOn } from "@/application/prefetch";
import { allocationsResource } from "@/entities/fund/model/fund-resource";
import { useUnreadCount, useUnreadCountPolling } from "@/entities/notification/model/notification-store";
import { useSignInDialog } from "@/features/auth/model/use-sign-in-dialog";
import { KycStatusChip } from "@/features/kyc";
import { useCabinetPathname } from "@/shared/lib/cabinet-route";
import { cn } from "@/shared/lib/cn";
import { SUPPORT_EMAIL } from "@/shared/config/support";
import type { AllocationList } from "@/shared/contracts";
import { type ResourceSeed, useSeededResource } from "@/shared/lib/resource";
import { visibleFor } from "@/shared/lib/roles";
import { useSession } from "@/shared/lib/use-session";
import { ProductIcon, productTone } from "@/shared/ui/icons/products";

interface NavItem {
  href: `/${string}`;
  /** The row's name in the reader's language. */
  text: (t: Translate) => string;
  icon: LucideIcon;
  active: (path: string) => boolean;
  /**
   * Session roles the row is shown to; absent means every role that sees the group. Set it
   * where the BFF answers a narrower set of roles than the group's — otherwise the row
   * leads to a screen made of 403s.
   */
  roles?: readonly string[];
}

// FUND group — the primary surfaces. "Home" is the portfolio dashboard; "Wallet" is the
// balance plus deposit/withdraw/activity, which the dashboard's money actions also link
// into. The mobile tab bar has no room for a sixth tab, so there /wallet still lights up
// Operations.
const FUND: NavItem[] = [
  { href: "/", text: (t) => t("nav.home", "Home"), icon: Home, active: (p) => p === "/" },
  // Exactly `/invest`, not everything beneath it: each product has its own row in
  // PRODUCTS, and a prefix match here lit both that row and this one on a product
  // page. Two highlighted rows is not a state the rail should be able to reach —
  // and with one marker per section it would mean two markers at once.
  { href: "/invest", text: (t) => t("nav.invest", "Invest"), icon: LineChart, active: (p) => p === "/invest" },
  { href: "/wallet", text: (t) => t("nav.wallet", "Wallet"), icon: Wallet, active: (p) => p.startsWith("/wallet") },
  { href: "/operations", text: (t) => t("nav.operations", "Activity"), icon: ListChecks, active: (p) => p.startsWith("/operations") },
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
//
// No dashboard row: fleet health and the relay KPIs are read in Grafana, not here. Users
// opens the group because it is the first thing an operator is asked to act on.
const ADMIN: NavItem[] = [
  { href: "/admin/users", text: (t) => t("nav.users", "Users"), icon: UsersRound, active: (p) => p.startsWith("/admin/users") },
  { href: "/admin/cabinet", text: (t) => t("nav.cabinet", "Cabinet"), icon: PanelsTopLeft, active: (p) => p.startsWith("/admin/cabinet") },
  { href: "/admin/treasury", text: (t) => t("nav.treasury", "Treasury"), icon: Landmark, active: (p) => p.startsWith("/admin/treasury") },
  { href: "/admin/coins", text: (t) => t("nav.coins", "Coins"), icon: Coins, active: (p) => p.startsWith("/admin/coins") },
  { href: "/admin/withdrawals", text: (t) => t("nav.withdrawals", "Withdrawals"), icon: ArrowUpFromLine, active: (p) => p.startsWith("/admin/withdrawals") },
  // Payments is where money is moved between the platform's claims and out to a chain —
  // every tier, authorised by whoever the money belongs to. Beside Withdrawals because
  // an external order becomes one.
  { href: "/admin/payments", text: (t) => t("nav.payments", "Payments"), icon: ArrowLeftRight, active: (p) => p.startsWith("/admin/payments") },
  // The third queue, after the two money queues: outbox rows the relay parked on a
  // terminal error, waiting for an operator to fix the cause and unpark them. The only
  // piece of the old dashboard that could not move to Grafana — unparking is an action.
  { href: "/admin/outbox", text: (t) => t("nav.outbox", "Outbox"), icon: Inbox, active: (p) => p.startsWith("/admin/outbox") },
  // Sits next to Treasury, not to Withdrawals: the question it answers is "what did the
  // fund earn", which belongs with the chart of accounts rather than with the user queue.
  // Statistics only since Payments took over the proposal; it links there and to Treasury.
  { href: "/admin/revenue", text: (t) => t("nav.revenue", "Revenue stats"), icon: PiggyBank, active: (p) => p.startsWith("/admin/revenue") },
  // Directly under Revenue stats, because the consilium is what authorizes money leaving
  // the fund — the page and the balance it governs read as one thought. Shown to every operator
  // session like its neighbours; only owners have a room to be in, and the BFF answers 403
  // to anyone else (the page renders that as its own state, not as an error).
  { href: "/consilium", text: (t) => t("nav.consilium", "Consilium"), icon: Gavel, active: (p) => p.startsWith("/consilium") },
  { href: "/admin/allocations", text: (t) => t("nav.allocations", "Allocations"), icon: Boxes, active: (p) => p.startsWith("/admin/allocations") },
  { href: "/admin/valuation", text: (t) => t("nav.valuation", "Valuation & redemptions"), icon: Receipt, active: (p) => p.startsWith("/admin/valuation") },
  // After Allocations, because a fee is a property OF a product: you register the fund
  // first and then price it. Distinct from Fund revenue, which is where the money ends up
  // once these terms have been charged and settled. Pricing a product is not an operator's
  // call: the BFF admits only admins and owners to `/api/admin/fees/*` (banking#269).
  { href: "/admin/fees", text: (t) => t("nav.fees", "Fees"), icon: Percent, active: (p) => p.startsWith("/admin/fees"), roles: ["admin", "owner"] },
];

// The bottom rail — the reader's own account, then the two things about it that change.
// Profile sits first: it is who the row is about, where Notifications and Settings are
// things done to that account. The mobile tab bar has no sixth slot, so below `lg` the
// profile is reached through Settings instead.
const SECONDARY: NavItem[] = [
  { href: "/profile", text: (t) => t("nav.profile", "Profile"), icon: UserRound, active: (p) => p.startsWith("/profile") },
  { href: "/notifications", text: (t) => t("nav.notifications", "Notifications"), icon: Bell, active: (p) => p.startsWith("/notifications") },
  { href: "/settings", text: (t) => t("nav.settings", "Settings"), icon: Settings, active: (p) => p.startsWith("/settings") },
];

// A product's row owns its page AND the surfaces under it — `/invest/<service>/trade` is
// the terminal over that product, not a different place in the rail. Matched with the
// trailing slash so `/invest/arb` never lights the row of a product called `arbitrage`.
function onProduct(pathname: string, service: string): boolean {
  const href = `/invest/${encodeURIComponent(service)}`;
  return pathname === href || pathname.startsWith(`${href}/`);
}

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
//
// `catalog` is the server's read of the product catalog: a seed, `undefined` when that read
// failed, or `null` while it is still in flight (the layout streams it under its own
// Suspense) — see `useSeededResource`.
export function Sidebar({ catalog }: { catalog?: ResourceSeed<AllocationList> | null }) {
  // Zone-relative, so the `active` predicates below can stay written in the paths
  // the app reasons in rather than the ones the browser shows.
  const t = useT();
  const pathname = useCabinetPathname();
  const session = useSession();
  const isAdmin = session?.user?.isAdmin ?? false;
  const guest = session !== null && !session.authenticated;
  const dialog = useSignInDialog();
  const role = session?.user?.role;
  const admin = ADMIN.filter((item) => visibleFor(item.roles, role));
  // The rail is mounted on every signed-in screen, so it is the one place the unread
  // count is polled from — every other consumer reads the shared store.
  useUnreadCountPolling(!guest);
  const unread = useUnreadCount();
  // The catalog is cached and mirrored to sessionStorage, so the rail lists its products on
  // the first frame of a return visit rather than growing a Products group a beat later. A
  // failed read leaves the group empty rather than blocking the rail — the nav is not the
  // place to surface an API error.
  const products = useSeededResource(allocationsResource, catalog).data?.allocations ?? [];
  // The row answers the click, not the RSC round-trip. Every cabinet route is dynamic
  // and none has a `loading.tsx`, so the pathname only changes once the new page's
  // payload has arrived — 150–300ms after the click, which is exactly the window in
  // which a highlight that has not moved reads as a click that did not land. The
  // clicked href is marked at once and the pathname catches up.
  //
  // The entry remembers the pathname it was made on, so the moment the pathname changes
  // it is stale by construction and the URL is the truth again. It is cleared during
  // render rather than in an effect — an effect would paint one frame with the URL's row
  // marked and the stale one not — and cleared rather than merely ignored: an entry left
  // behind would come back to life on a return to the page it was made on, marking
  // Operations again on the way back from it to Wallet. A click that never navigates
  // (cancelled, failed) is forgotten by the next one.
  const [pending, setPending] = useState<{ href: `/${string}`; from: string } | null>(null);
  if (pending && pending.from !== pathname) setPending(null);
  const marked = pending && pending.from === pathname ? pending.href : pathname;
  const mark = (href: `/${string}`) => setPending({ href, from: pathname });

  return (
    <aside className="flex h-full w-[var(--cabinet-rail-w)] flex-col gap-7 overflow-y-auto border-r border-border bg-secondary px-4.5 pb-5 pt-6">
      <nav aria-label={t("nav.a11y.primary", "Primary")} className="flex flex-col gap-4.5">
        <Section label={t("nav.group.fund", "Your account")} at={FUND.some((i) => i.active(marked)) ? marked : null}>
          {FUND.map((item) => (
            <NavLink key={item.href} item={item} active={item.active(marked)} onClick={onRailClick(item.href, mark)} />
          ))}
        </Section>
        {products.length > 0 && (
          <Section label={t("invest.products", "Products")} at={products.some((p) => onProduct(marked, p.service)) ? marked : null}>
            {products.map((p) => {
              // Every row pointed at `/invest`, so naming a product in the rail took you to
              // the list of all of them. The product's own page is keyed by its service id.
              const href: `/${string}` = `/invest/${encodeURIComponent(p.service)}`;
              const active = onProduct(marked, p.service);
              return (
                <Link
                  key={p.service}
                  href={href}
                  {...prefetchOn(href)}
                  onClick={onRailClick(href, mark)}
                  aria-current={active ? "page" : undefined}
                  className={cn(
                    "flex items-center gap-3 rounded-lg px-3 py-2 text-sm transition-colors",
                    NAV_FOCUS,
                    active ? "font-semibold text-on-primary" : "font-medium text-ink hover:bg-ink/5",
                  )}
                >
                  <span className={cn("flex size-5 shrink-0 items-center justify-center rounded-md", productTone(p.service))}>
                    <ProductIcon icon={p.icon} className="size-3.5" />
                  </span>
                  <span className="truncate">{p.title}</span>
                </Link>
              );
            })}
          </Section>
        )}
        {isAdmin && (
          <Section label={t("admin.eyebrow.administer", "Administer")} at={admin.some((i) => i.active(marked)) ? marked : null}>
            {admin.map((item) => (
              <NavLink key={item.href} item={item} active={item.active(marked)} onClick={onRailClick(item.href, mark)} />
            ))}
          </Section>
        )}
      </nav>

      <div className="flex-1" />

      <nav aria-label={t("nav.a11y.secondary", "Secondary")} className="flex flex-col">
        <Section at={SECONDARY.some((i) => i.active(marked)) ? marked : null}>
          {SECONDARY.map((item) => {
            const active = item.active(marked);
            // The two rows about the account carry its two live facts: the verification
            // state on Profile, where the row that changes it lives (#395), and the unread
            // count on Notifications.
            const trailing = guest ? undefined : item.href === "/profile" ? <KycStatusChip active={active} /> : item.href === "/notifications" && unread ? <UnreadPill count={unread} active={active} /> : undefined;
            return <NavLink key={item.href} item={item} active={active} onClick={onRailClick(item.href, mark)} trailing={trailing} />;
          })}
          <SupportLink />
          {guest && (
            // A guest's way in, wherever they are: the same dialog every wall opens.
            <button
              type="button"
              onClick={dialog.show}
              className={cn("mt-2 inline-flex items-center justify-center rounded-full border border-primary-ink px-3 py-1.5 font-mono text-xs uppercase tracking-wider text-primary-ink hover:bg-primary hover:text-on-primary", NAV_FOCUS)}
            >
              {t("nav.guestMode", "Guest Mode")}
            </button>
          )}
        </Section>
      </nav>
    </aside>
  );
}

// A modified or non-primary click opens the page somewhere else — a new tab, a window,
// the context menu — and `next/link` lets the browser have it. The row the reader is
// looking at is still the current one, so it keeps the mark. `defaultPrevented` is the
// same courtesy to anything upstream that claimed the click first.
function onRailClick(href: `/${string}`, mark: (href: `/${string}`) => void) {
  return (e: MouseEvent<HTMLAnchorElement>) => {
    if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0) return;
    mark(href);
  };
}

// One rail section: an optional eyebrow, its rows, and — while one of those rows is
// marked — the single highlight that sits behind it.
//
// The highlight is one node per SECTION, mounted once and moved, the way the mobile tab
// bar's rule is. It used to be a `layoutId` pill inside the marked row, and that had two
// costs. The slide was a JS-driven layout projection competing for the main thread with
// the new page's render, so it dropped frames on precisely the navigation it was meant
// to smooth. And its origin was wrong: motion's page-box measurement adds `window.scroll`
// to every box unless the element itself is `position: fixed`, so the unmount snapshot
// carried the old page's scrollY while the new pill was measured after Next had scrolled
// to the top — the pill set off from `scrollY` pixels below its row. Here the move is a
// CSS transition on `transform`, which runs on the compositor and survives whatever the
// main thread is doing, and the origin is wherever the marker already is.
//
// One per section rather than one per rail for the same reason as before: a marker
// travelling from Invest to Users would cross two headings that have nothing to do with
// either row, and the distance would imply a relationship that does not exist. Crossing
// a section unmounts one marker and mounts another, which is a fade.
//
// The tab bar positions its marker by arithmetic because its tabs are uniform; rail rows
// are not — sections differ in row count and every label is a translation — so this one
// is placed by measurement. SSR cannot know those offsets, so until the first placement
// the marked row carries the fill itself (globals.css) and the marker stays hidden; one
// forced layout read per navigation is the price of the move.
//
// `isolate` is load-bearing: the marker sits on a negative z-index, and without a
// stacking context of its own it would land behind the rail's background instead of
// behind the row's label.
function Section({ label, at, children }: { label?: string; at: string | null; children: ReactNode }) {
  const root = useRef<HTMLDivElement>(null);
  // Which marker node was last placed, so a re-run over the same node — a move within
  // the section, a resize — is told apart from a marker that has just mounted. Starts
  // undefined rather than null so the first run is told apart from a run that found no
  // marker: only a marker mounting AFTER the section has settled is entering it and
  // fades; the one hydration finds already on its row cuts straight in.
  const placed = useRef<HTMLElement | null | undefined>(undefined);
  useLayoutEffect(() => {
    const section = root.current;
    if (!section) return;
    const place = () => {
      const marker = section.querySelector<HTMLElement>('[data-slot="rail-marker"]');
      const row = section.querySelector<HTMLElement>('[aria-current="page"]');
      const fresh = marker !== placed.current;
      const entering = fresh && placed.current !== undefined;
      placed.current = marker;
      if (!marker || !row) return;
      // Offsets are relative to the section (its `relative`), so they land on the row
      // whatever the rail around it is doing.
      if (fresh) marker.style.transition = "none";
      marker.style.transform = `translate(${row.offsetLeft}px, ${row.offsetTop}px)`;
      marker.style.width = `${row.offsetWidth}px`;
      marker.style.height = `${row.offsetHeight}px`;
      if (fresh) {
        // A new node has already been styled once — by the layout read above — at the
        // section's origin, and a transition is decided by the style AFTER the change.
        // Flushing the placement first makes it the starting point rather than the
        // destination, so the marker appears on its row instead of sliding in from the
        // top of the section.
        void marker.offsetWidth;
        marker.style.transition = "";
        marker.dataset.placed = entering ? "entered" : "first";
      }
    };
    place();
    // Rows come and go (the catalog loads, a role hides Fees) and labels change width
    // with the locale or a font swap; any of it moves the marked row without changing
    // which row is marked.
    const observer = new ResizeObserver(place);
    observer.observe(section);
    return () => observer.disconnect();
  }, [at]);
  return (
    <div ref={root} className={cn("relative isolate flex flex-col gap-1", label !== undefined && "pl-1")}>
      {/* First in DOM order: the pre-hydration fallback in globals.css reaches the marked
          row through a sibling combinator, which only looks forward. */}
      {at !== null && <span data-slot="rail-marker" aria-hidden className="pointer-events-none absolute left-0 top-0 -z-10 rounded-lg bg-primary" />}
      {label !== undefined && <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{label}</p>}
      {children}
    </div>
  );
}

function NavLink({
  item,
  active,
  onClick,
  trailing,
}: {
  item: NavItem;
  active: boolean;
  onClick: (e: MouseEvent<HTMLAnchorElement>) => void;
  trailing?: ReactNode;
}) {
  const t = useT();
  const Icon = item.icon;
  return (
    <Link
      href={item.href}
      {...prefetchOn(item.href)}
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={cn(
        "flex items-center gap-3 rounded-lg px-3 py-2 text-sm transition-colors",
        NAV_FOCUS,
        active ? "font-semibold text-on-primary" : "font-medium text-ink hover:bg-ink/5",
      )}
    >
      <Icon className="size-4.5 shrink-0" />
      {/* `min-w-0` for the same reason as the mobile tab bar: without it the
          label refuses to shrink below its own text, and the rail widens to fit
          the longest translation instead of the label truncating inside it —
          German "Benachrichtigungen" and "Bewertung & Rücknahmen" are both wider
          than the rail. `title` keeps the full label reachable. */}
      <span className="min-w-0 flex-1 truncate" title={item.text(t)}>
        {item.text(t)}
      </span>
      {trailing}
    </Link>
  );
}

// The way to a person, last in the rail. A mail link rather than a NavItem: there is no
// support page to be "on", so it never takes the marker, and the address is the one every
// KYC dead end already offers. The mobile tab bar has no slot for it — there the same row
// sits under Settings (#385).
function SupportLink() {
  const t = useT();
  return (
    <a href={`mailto:${encodeURIComponent(SUPPORT_EMAIL)}`} className={cn("flex items-center gap-3 rounded-lg px-3 py-2 text-sm font-medium text-ink transition-colors hover:bg-ink/5", NAV_FOCUS)}>
      <LifeBuoy className="size-4.5 shrink-0" />
      <span className="min-w-0 flex-1 truncate" title={t("nav.support", "Support")}>
        {t("nav.support", "Support")}
      </span>
    </a>
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
        active ? "bg-background text-ink" : "bg-primary-ink/15 text-primary-ink",
      )}
    >
      {count > 99 ? "99+" : count}
    </span>
  );
}
