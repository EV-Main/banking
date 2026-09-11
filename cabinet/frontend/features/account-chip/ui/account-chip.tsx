"use client";

import { BadgeCheck, LogOut } from "lucide-react";
import { useEffect, useState } from "react";

import { translator, type Translate } from "@evinvest/i18n";

import { profileResource } from "@/entities/user/model/profile-resource";
import { cabinetPath } from "@/shared/config/base-path";
import { chipMessages } from "@/features/account-chip/model/chip-messages";
import { cn } from "@/shared/lib/cn";
import { csrfHeader } from "@/shared/lib/csrf-client";
import { documentLocale } from "@/shared/lib/locale-cookie";
import { clearResources, useResource } from "@/shared/lib/resource";
import { SESSION_UNAVAILABLE, useSession } from "@/shared/lib/use-session";

import { clearIdentity, readIdentity, writeIdentity } from "../model/identity-cache";

// The chip's three controls are hand-written so the bundle stays free of the uikit Button,
// which means the keyboard focus ring has to be written out too.
const CHIP_FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-ring";

/**
 * The chip's own translator.
 *
 * `useT()` cannot be used here and the reason is the same one that rules out next/link
 * below: this bundle mounts on the CONDUCTOR origin, where none of the cabinet's React
 * tree — and so no `I18nProvider` — exists above it. The hook would throw. It reads the
 * locale the way it already reads it for every link it builds, off the document it was
 * mounted into, and constructs a translator over `chip-messages.ts` — the four strings
 * this component renders, and nothing else. Not the cabinet's `messagesFor()`: that pulls
 * all five `common.json` files and runs the resolve policy over them at module scope,
 * which put 532 KB of catalogue into a bundle the conductor injects on every page of the
 * public site. An anonymous visitor was downloading the German admin-treasury copy to
 * render the word "Verified". `chip-messages.test.ts` keeps the small copy in step with
 * the real catalogues, so there is still one source of truth — just not one bundle.
 *
 * Not memoised, and deliberately not: `documentLocale()` is two property reads and
 * `translator()` closes over a catalogue it does not copy, so caching the pair would cost
 * more in the machinery than it saves — and a cached one would go stale the moment the
 * conductor swapped the document's language under a chip that never remounts.
 */
function chipTranslator(): Translate {
  const locale = documentLocale();
  return translator(chipMessages(locale), locale);
}

// The account chip, rendered as a cabinet microfrontend inside the conductor's shared
// header (registered in site_conductor's mfe-registry as `cabinet.account`). It replaces
// the header's old Investor Portal button and owns all three states itself:
//   • loading      → a compact skeleton
//   • authenticated → avatar + name + Verified + sign-out
//   • signed-out   → the Investor Portal CTA (so anonymous marketing-site visitors still
//                     get a way into the cabinet)
//
// Framework-agnostic on purpose: the bundle mounts as a vanilla-React custom element on
// the CONDUCTOR origin, which has no cabinet Next router — so no next/link or
// next/navigation, and no I18nProvider either (see chipTranslator above). Every
// destination is a cross-zone hard <a href> (PATTERNS §9), routed back into the zone via
// cabinetPath(). And unlike the sidebar chip it NEVER redirects on a dropped session: an
// anonymous visitor on the public site must not be bounced to login.
//
// Those destinations are LOCALISED, and that is the whole fix for "the cabinet always opens
// in Russian". Cabinet page URLs carry the locale first (`/{locale}/cabinet/profile`); the
// chip used to link to the locale-free `/cabinet/profile`, which the zone proxy then had to
// resolve on its own — and with nothing carrying the reader's site language across the zone
// boundary, it resolved from Accept-Language. A reader on the English landing with a
// Russian-configured browser was sent to /ru/cabinet/profile every time. The document the
// chip is rendered into already knows the answer, so it links straight there and no guess
// is ever made. site_conductor also mirrors that locale into the `ev_locale` cookie, which
// covers the other entry points (bookmarks, old links) the chip is not involved in.
export function AccountChip({ className }: { className?: string }) {
  const session = useSession();
  // Read once, at mount: the optimistic identity from the last visit.
  const [identity] = useState(readIdentity);
  const email = session?.user?.email ?? null;

  // Invalidate the cache only on a GENUINE signed-out result. A transient
  // session-endpoint blip resolves to the frozen SESSION_UNAVAILABLE sentinel — treating
  // that as sign-out would wipe the optimization and flash the CTA at a still-logged-in
  // user on the next load.
  const signedOut =
    session !== null && !session.authenticated && session !== SESSION_UNAVAILABLE;
  useEffect(() => {
    if (signedOut) clearIdentity();
  }, [signedOut]);

  // Still loading: trust the cache if we have one (same tab ⇒ same user in practice) so
  // the chip paints its final form now — but don't persist from here, the email isn't
  // confirmed yet. Never the email-derived name.
  if (session === null) {
    return identity ? (
      <AuthedChip
        className={className}
        email={identity.email}
        seedName={identity.name}
        persist={false}
      />
    ) : (
      <ChipSkeleton className={className} />
    );
  }
  if (!session.authenticated) return <SignInCta className={className} />;
  // Confirmed session: trust the seeded name only if it belongs to THIS account, so a
  // same-tab account switch never paints (or re-persists) the previous user's name.
  const seedName = identity?.email === email ? identity.name : null;
  return <AuthedChip className={className} email={email} seedName={seedName} persist />;
}

function AuthedChip({
  className,
  email,
  seedName,
  persist,
}: {
  className?: string;
  email: string | null;
  seedName: string | null;
  persist: boolean;
}) {
  // The profile refines the label (preferred/legal name); it only fetches here, in the
  // authenticated branch.
  // `isLoading` is false once the read has settled either way, so a profile that failed to
  // load falls through to the heuristic below instead of holding the skeleton forever.
  const { data: profile, isLoading } = useResource(profileResource);
  const t = chipTranslator();
  const settled = !isLoading;
  const realName = profile?.preferred_name || profile?.legal_name || null;
  // Resolution order: live profile name → the name seeded from last visit → (only once the
  // fetch has settled without a preferred/legal name) the email heuristic. Showing the
  // email-derived label while the profile is still loading is the flicker we avoid — it
  // would visibly correct itself a beat later. Until a name is known, render a skeleton,
  // not a name we'll replace. Truncate long names so the chip doesn't push the central nav
  // off-centre.
  const name = truncate(realName ?? seedName ?? (settled ? displayName(email, t) : null));

  useEffect(() => {
    // Persist only from a confirmed session (persist) and only real names, so the cache
    // never holds a heuristic or a name that disagrees with its email.
    if (persist && realName) writeIdentity({ email, name: realName });
  }, [persist, email, realName]);

  async function signOut() {
    // Shell-owned logout (site-root /api/auth): revokes the shared session and clears
    // its cookies for every zone at once.
    await fetch("/api/auth/logout", { method: "POST", headers: csrfHeader() });
    // The reads cached for this account go with the session. The hard navigation below
    // clears the in-memory half anyway; this is what clears the sessionStorage half.
    clearResources();
    window.location.href = cabinetPath(documentLocale(), "/loggedout");
  }

  return (
    <div className={cn("flex items-center gap-2", className)}>
      <a
        href={cabinetPath(documentLocale(), "/profile")}
        className={cn("flex min-w-0 items-center gap-2.5 rounded-lg px-1.5 py-1 transition-colors hover:bg-ink/5", CHIP_FOCUS)}
      >
        <span className="flex size-8.5 shrink-0 items-center justify-center rounded-full bg-accent-debug/15 text-xs font-semibold text-accent-debug">
          {initialsOf(email)}
        </span>
        <div className="min-w-0">
          {name ? (
            <p className="truncate text-sm font-semibold text-ink">{name}</p>
          ) : (
            <span className="my-1 block h-3 w-24 animate-pulse rounded bg-ink/10" aria-hidden />
          )}
          {/* i18n-max: 12 — sits under the name inside the chip's `min-w-0` column, which
              is what the conductor's central nav is centred against. */}
          <p className="flex items-center gap-1 text-xs font-medium text-accent-debug">
            <BadgeCheck className="size-3 shrink-0" /> {t("ui.verified")}
          </p>
        </div>
      </a>
      <button
        type="button"
        onClick={signOut}
        aria-label={t("auth.signOut")}
        className={cn("shrink-0 rounded-md text-ink-soft transition-colors hover:text-ink", CHIP_FOCUS)}
      >
        <LogOut className="size-4" />
      </button>
    </div>
  );
}

// Signed-out (or BFF-unavailable) state — the Investor Portal CTA the chip supersedes.
// Styled to match the conductor's old InvestorPortalButton (uikit outline) without pulling
// the uikit Button into the bundle. Links into the cabinet zone's sign-in.
function SignInCta({ className }: { className?: string }) {
  const t = chipTranslator();
  return (
    // i18n-max: 20 — a fixed-height pill in the conductor's header row, `px-4` and no
    // truncation, sharing that row with the site nav.
    <a
      href={cabinetPath(documentLocale(), "/login")}
      className={cn(
        "inline-flex h-9 items-center justify-center rounded-md border border-accent-debug bg-transparent px-4 font-mono-tech text-xs tracking-wider text-accent-debug transition-all duration-300 hover:bg-primary hover:text-primary-foreground",
        CHIP_FOCUS,
        className,
      )}
    >
      {t("auth.investorPortal")}
    </a>
  );
}

function ChipSkeleton({ className }: { className?: string }) {
  return (
    <div className={cn("flex items-center gap-2.5 px-1.5 py-1", className)} aria-hidden>
      <span className="size-8.5 shrink-0 animate-pulse rounded-full bg-ink/10" />
      <span className="h-3 w-20 animate-pulse rounded bg-ink/10" />
    </div>
  );
}

// Takes the translator rather than reaching for one: it is a pure function over an email
// and stays that way, and its one word of prose is the last-resort label.
function displayName(email: string | null | undefined, t: Translate): string {
  if (email === undefined) return "…";
  if (!email) return t("ui.account");
  const handle = email.split("@")[0] ?? email;
  const parts = handle.split(/[._-]+/).filter(Boolean);
  const first = parts[0] ? cap(parts[0]) : handle;
  const last = parts[1] ? `${parts[1][0]!.toUpperCase()}.` : "";
  return [first, last].filter(Boolean).join(" ");
}

function initialsOf(email: string | null | undefined): string {
  if (!email) return "EV";
  const parts = (email.split("@")[0] ?? "").split(/[._-]+/).filter(Boolean);
  const a = parts[0]?.[0] ?? email[0] ?? "E";
  const b = parts[1]?.[0] ?? "";
  return (a + b).toUpperCase();
}

function cap(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** Keep the chip label from pushing the central nav off-centre. */
function truncate(s: string | null, max = 28): string | null {
  if (!s) return s;
  if (s.length <= max) return s;
  return `${s.slice(0, max - 1)}…`;
}
