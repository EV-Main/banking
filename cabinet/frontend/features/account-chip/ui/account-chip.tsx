"use client";

import { useEffect, useMemo, useState, useSyncExternalStore } from "react";

import {
  clearIdentity,
  identityOf,
  parseIdentity,
  profileName,
  readIdentityRaw,
  seedFor,
  subscribeIdentity,
  writeIdentity,
  type CachedIdentity,
} from "@/entities/user/model/identity-cache";
import { cabinetPath, isCabinetPage } from "@/shared/config/base-path";
import { platformAccountHref } from "@/shared/config/platform";
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
import { performSignOut } from "../model/sign-out";
import { AccountMenu, CHIP_FOCUS, type MenuIdentity, type MenuLinks } from "./account-menu";

// Inlined by `mfe/build.mjs`; unset, the menu offers no Service-Arb. Not through `config`:
// that reads every variable off `process`, which this bundle does not have on the host page.
// eslint-disable-next-line no-restricted-properties
const SA_PANEL_URL = process.env.NEXT_PUBLIC_SA_PANEL_URL;
// Inlined the same way; the account itself lives in concierge, on the platform origin.
// eslint-disable-next-line no-restricted-properties
const PLATFORM_URL = process.env.NEXT_PUBLIC_PLATFORM_URL;

/** Keeps the label from pushing the host's central nav off-centre. */
const MAX_LABEL = 28;

// The account chip in cabinet mode — on the public site and in the cabinet zone, where the
// cabinet's own session cookie is readable. It owns the session, the profile read and the
// logout; `AccountMenu` only draws. States:
//   • no cache, session loading            → AccountMenu skeleton (final box, w-24 bar)
//   • cache, session loading or unreachable → the cached chip, so a blip never shows the CTA;
//                                             locked — no email, nothing opens — until the
//                                             session vouches for it (a shared machine)
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
    // Unconfirmed: paint the last visit's name rather than a skeleton or the CTA, but not
    // its email and not its menu — that cache may be someone else's.
    if (cached) return <CabinetMenu className={className} identity={{ ...menuIdentity(cached), email: null }} locked />;
    if (session === null) return <CabinetMenu className={className} identity={null} />;
  }
  if (!session?.authenticated) return <SignInCta className={className} intent={intent} returnTo={returnTo} />;
  const email = session.user?.email ?? null;
  return <AuthedChip className={className} email={email} seed={seedFor(cached, email)} />;
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
function CabinetMenu({ className, identity, locked }: { className?: string; identity: MenuIdentity | null; locked?: boolean }) {
  const t = chipTranslator();
  const [failed, setFailed] = useState(false);
  const onSignOut = async () => {
    setFailed(false);
    if (!(await signOut())) setFailed(true);
  };
  return (
    <AccountMenu
      className={className}
      identity={identity}
      locked={locked}
      links={cabinetLinks()}
      groups={serviceGroups()}
      error={failed ? t("auth.err.signOut", "We couldn't sign you out. Please try again.") : null}
      onSwitchAccount={clearIdentity}
      onSignOut={() => void onSignOut()}
    />
  );
}

function signOut(): Promise<boolean> {
  return performSignOut({
    // Wrapped: a bare `window.fetch` reference called off `window` throws "Illegal invocation".
    fetch: (input, init) => fetch(input, init),
    csrfHeader,
    clearIdentity,
    clearResources,
    navigate: (href) => {
      window.location.href = href;
    },
    locale: documentLocale(),
  });
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
    profile: inCabinet ? { href: platformAccountHref(PLATFORM_URL, "profile"), kind: "profile" } : { href: cabinetPath(locale, "/"), kind: "cabinet" },
    manage: platformAccountHref(PLATFORM_URL, "security"),
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
// must be a core utility or a `tokens.css` token, or the pill would change with its host;
// the colours the hover changes start as arbitrary properties so a host's own
// primary-ink text utility in a higher layer cannot pin them (chip-parts.ts).
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
        "inline-flex h-9 items-center justify-center rounded-md! border! [border-color:var(--color-primary-ink)]! [background-color:transparent]! px-4! font-mono text-xs tracking-wider [color:var(--color-primary-ink)]! tabular-nums transition-all hover:border-primary! hover:bg-primary! hover:text-on-primary!",
        CHIP_FOCUS,
        className,
      )}
    >
      {t("auth.cabinet", "Cabinet")}
    </a>
  );
}
