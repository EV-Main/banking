"use client";

import { useEffect, useMemo, useSyncExternalStore } from "react";

import {
  clearIdentity,
  identityOf,
  parseIdentity,
  profileName,
  readIdentityRaw,
  subscribeIdentity,
  writeIdentity,
  type CachedIdentity,
} from "@/entities/user/model/identity-cache";
import { cabinetPath, isCabinetPage } from "@/shared/config/base-path";
import { cn } from "@/shared/lib/cn";
import { csrfHeader } from "@/shared/lib/csrf-client";
import { displayName, initialsOfName, truncateName } from "@/shared/lib/identity";
import { documentLocale } from "@/shared/lib/locale-cookie";
import { clearResources, useResource } from "@/shared/lib/resource";
import { SESSION_UNAVAILABLE, useSession } from "@/shared/lib/use-session";

import { chipTranslator } from "../lib/chip-translator";
import type { MenuGroup } from "../lib/host-attributes";
import { signInHref } from "../lib/sign-in-href";
import { chipProfileResource } from "../model/chip-profile";
import { AccountMenu, CHIP_FOCUS, type MenuIdentity, type MenuLinks } from "./account-menu";

// Inlined by `mfe/build.mjs`; unset, the menu offers no Service-Arb. Not through `config`:
// that reads every variable off `process`, which this bundle does not have on the host page.
// eslint-disable-next-line no-restricted-properties
const SA_PANEL_URL = process.env.NEXT_PUBLIC_SA_PANEL_URL;

/** Keeps the label from pushing the host's central nav off-centre. */
const MAX_LABEL = 28;

// The account chip in cabinet mode — on the public site and in the cabinet zone, where the
// cabinet's own session cookie is readable. It owns the session, the profile read and the
// logout; `AccountMenu` only draws. States:
//   • no cache, session loading            → AccountMenu skeleton (final box, w-24 bar)
//   • cache, session loading or unreachable → the cached chip, so a blip never shows the CTA
//   • signed in                             → live chip; profile name refines the label
//   • signed out                            → the Cabinet CTA
//
// Framework-agnostic: no next/link or next/navigation (no cabinet router on the conductor
// origin). Destinations are hard <a href>s built with cabinetPath() for the document's own
// locale, so the reader lands in the language they were reading. It NEVER redirects on a
// dropped session: an anonymous visitor on the public site must not be bounced to login.
export type AccountChipProps = {
  className?: string;
  /** `data-intent` on the element: `signup` | `login`; anything else is ignored. */
  intent?: string | null;
  /** `data-return-to` on the element: a zone-relative path, same-origin checked. */
  returnTo?: string | null;
};

export function AccountChip({ className, intent, returnTo }: AccountChipProps) {
  const session = useSession();
  const raw = useSyncExternalStore(subscribeIdentity, readIdentityRaw, () => null);
  const cached = useMemo(() => parseIdentity(raw), [raw]);

  // Only a GENUINE signed-out answer clears the cache; SESSION_UNAVAILABLE is a blip.
  const signedOut = session !== null && session !== SESSION_UNAVAILABLE && !session.authenticated;
  useEffect(() => {
    if (signedOut) clearIdentity();
  }, [signedOut]);

  if (session === null || session === SESSION_UNAVAILABLE) {
    // Unconfirmed: paint the last visit's identity rather than a skeleton or the CTA.
    if (cached) return <CabinetMenu className={className} identity={menuIdentity(cached)} />;
    if (session === null) return <CabinetMenu className={className} identity={null} />;
  }
  if (!session?.authenticated) return <SignInCta className={className} intent={intent} returnTo={returnTo} />;
  const email = session.user?.email ?? null;
  // A cache from another account (a switch in this browser) is ignored, never shown.
  const seed = cached && cached.email === email ? cached : null;
  return <AuthedChip className={className} email={email} seed={seed} />;
}

/** Label and initials from `name ?? email` ("Valentina Kuznetsova" → "VK"). */
function labelled(email: string | null, name: string | null): MenuIdentity {
  return {
    email,
    initials: initialsOfName(name ?? "", email),
    label: truncateName(name ?? displayName(email, chipTranslator()), MAX_LABEL),
  };
}

function menuIdentity(cached: CachedIdentity): MenuIdentity {
  return { ...labelled(cached.email, cached.name), initials: cached.initials };
}

function AuthedChip({ className, email, seed }: { className?: string; email: string | null; seed: CachedIdentity | null }) {
  const { data: profile, isLoading } = useResource(chipProfileResource);
  // undefined: not read yet; null: read, and the profile has no name.
  const name = profile ? profileName(profile) : undefined;
  useEffect(() => {
    // Confirmed session + live profile only; writeIdentity itself skips an unchanged value.
    if (email && name !== undefined) writeIdentity(identityOf(email, name));
  }, [email, name]);

  // The profile is the truth once it lands; until then the matching cache; once the read has
  // settled without one (failed), the email heuristic. Never the heuristic while the profile
  // is in flight — it would visibly correct itself a beat later.
  const identity =
    name !== undefined ? labelled(email, name) : seed ? menuIdentity(seed) : isLoading ? null : labelled(email, null);
  return <CabinetMenu className={className} identity={identity} />;
}

/** AccountMenu with the cabinet's own links, groups and logout. */
function CabinetMenu({ className, identity }: { className?: string; identity: MenuIdentity | null }) {
  return (
    <AccountMenu
      className={className}
      identity={identity}
      links={cabinetLinks()}
      groups={serviceGroups()}
      onSwitchAccount={clearIdentity}
      onSignOut={signOut}
    />
  );
}

async function signOut() {
  clearIdentity();
  // Shell-owned logout (site-root /api/auth): revokes the shared session and clears its
  // cookies for every zone at once.
  await fetch("/api/auth/logout", { method: "POST", headers: csrfHeader() });
  // The hard navigation below clears the in-memory reads anyway; this clears the persisted ones.
  clearResources();
  window.location.href = cabinetPath(documentLocale(), "/loggedout");
}

/**
 * Read at render, like the locale — the conductor swaps documents under a chip that never
 * remounts. The header opens the cabinet home from the public site (an investor arriving
 * from the landing wants their portfolio) and the profile from inside the cabinet.
 */
function cabinetLinks(): MenuLinks {
  const locale = documentLocale();
  const here = typeof document === "undefined" ? "/" : document.location.pathname + document.location.search;
  const inCabinet = typeof document !== "undefined" && isCabinetPage(document.location.pathname);
  return {
    profile: cabinetPath(locale, inCabinet ? "/profile" : "/"),
    manage: cabinetPath(locale, "/settings"),
    // Google's chooser is on every sign-in; the new session closes this one.
    switchAccount: `/api/auth/login?returnTo=${encodeURIComponent(here)}`,
  };
}

/** The Services group; Service-Arb is `current` when this page is the panel's own origin. */
function serviceGroups(): MenuGroup[] {
  if (!SA_PANEL_URL) return [];
  let current = false;
  try {
    current = typeof location !== "undefined" && location.origin === new URL(SA_PANEL_URL).origin;
  } catch {
    // A malformed build-time URL only loses the highlight.
  }
  const t = chipTranslator();
  return [
    {
      id: "services",
      label: t("ui.services", "Services"),
      items: [{ id: "service-arb", href: SA_PANEL_URL, label: "Service-Arb", external: false, current }],
    },
  ];
}

// Signed-out (or BFF-unavailable with no cache) — the sign-in CTA, labelled "Cabinet" (#399).
//
// A primary-ink outline pill by hand rather than the uikit `Button`: the kit's class strings
// live in node_modules, which the Tailwind scan behind `mfe.css` never reads, and
// `@source`-ing the kit's dist doubles the chip CSS on every public page. Every class here
// must be a core utility or a `tokens.css` token, or the pill would change with its host.
//
// Not instrumented: the `cta_clicked` event is captured by the HOST (site_conductor's
// `CabinetEntryTracker`, keyed on `data-cta`). If the chip ever fires its own, the host side
// must stop, or the funnel double-counts.
function SignInCta({ className, intent, returnTo }: AccountChipProps) {
  const t = chipTranslator();
  return (
    // i18n-max: 20 — a fixed-height pill in the conductor's header row, `px-4` and no
    // truncation, sharing that row with the site nav.
    <a
      href={signInHref(documentLocale(), { intent, returnTo })}
      data-cta="cabinet"
      className={cn(
        "inline-flex h-9 items-center justify-center rounded-md border border-primary-ink bg-transparent px-4 font-mono text-xs tracking-wider text-primary-ink tabular-nums transition-all hover:border-primary hover:bg-primary hover:text-on-primary",
        CHIP_FOCUS,
        className,
      )}
    >
      {t("auth.cabinet", "Cabinet")}
    </a>
  );
}
