"use client";

import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";

import { Gavel, Home, LifeBuoy, Wallet, type LucideIcon } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { type MouseEvent, type ReactNode, useLayoutEffect, useRef, useState } from "react";

import { prefetchOn } from "@/application/prefetch";
import { useSignInDialog } from "@/features/auth/model/use-sign-in-dialog";
import { useCabinetPathname } from "@/shared/lib/cabinet-route";
import { cn } from "@/shared/lib/cn";
import { SUPPORT_EMAIL } from "@/shared/config/support";
import { useSession } from "@/shared/lib/use-session";

interface NavItem {
  href: `/${string}`;
  /** The row's name in the reader's language. */
  text: (t: Translate) => string;
  icon: LucideIcon;
  active: (path: string) => boolean;
}

// FUND group — the two thin hosts that remain after banking#565 (investor screens moved to
// the invest cabinet). Both pages render outside this shell, without the rail.
const FUND: NavItem[] = [
  { href: "/", text: (t) => t("nav.home", "Home"), icon: Home, active: (p) => p === "/" },
  { href: "/wallet", text: (t) => t("nav.wallet", "Wallet"), icon: Wallet, active: (p) => p.startsWith("/wallet") },
];

// ADMINISTER group — what is left of the operator console here until invest#37: the
// consilium. Rendered only for a non-investor session role (the BFF's `/api/auth/session`
// `isAdmin`); the screen is also authorized server-side, so hiding the row is cosmetic,
// not the security boundary. Only owners have a room to be in, and the BFF answers 403 to
// anyone else (the page renders that as its own state, not as an error).
const ADMIN: NavItem[] = [
  { href: "/consilium", text: (t) => t("nav.consilium", "Consilium"), icon: Gavel, active: (p) => p.startsWith("/consilium") },
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
// rail scrolls internally solely when it can't fit (a short viewport), so nothing gets clipped.
export function Sidebar() {
  // Zone-relative, so the `active` predicates below can stay written in the paths
  // the app reasons in rather than the ones the browser shows.
  const t = useT();
  const pathname = useCabinetPathname();
  const session = useSession();
  const isAdmin = session?.user?.isAdmin ?? false;
  const guest = session !== null && !session.authenticated;
  const dialog = useSignInDialog();
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
  // Home again on the way back from it to Wallet. A click that never navigates
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
        {isAdmin && (
          <Section label={t("admin.eyebrow.administer", "Administer")} at={ADMIN.some((i) => i.active(marked)) ? marked : null}>
            {ADMIN.map((item) => (
              <NavLink key={item.href} item={item} active={item.active(marked)} onClick={onRailClick(item.href, mark)} />
            ))}
          </Section>
        )}
      </nav>

      <div className="flex-1" />

      <nav aria-label={t("nav.a11y.secondary", "Secondary")} className="flex flex-col">
        <Section at={null}>
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
// travelling from Wallet to Consilium would cross two headings that have nothing to do with
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
    // Rows come and go (the session decides the admin group) and labels change width
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
}: {
  item: NavItem;
  active: boolean;
  onClick: (e: MouseEvent<HTMLAnchorElement>) => void;
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
          the longest translation instead of the label truncating inside it.
          `title` keeps the full label reachable. */}
      <span className="min-w-0 flex-1 truncate" title={item.text(t)}>
        {item.text(t)}
      </span>
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

