"use client";

// The two parts every presentation of a verification start is built from: the control that
// launches it, and the live region that reports what came back. Held here rather than in
// each presentation so the profile row and the wallet block cannot drift apart on what a
// 503, a 429 or a stale token means — the wording of an outcome is decided once.

import { useT } from "@evinvest/i18n/react";

import type { ReactNode } from "react";

import { Button, type ButtonSize, Spinner } from "@evinvest/uikit";

import type { StartState, StartVerification } from "@/features/kyc/model/use-start-verification";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { SupportLink } from "@/shared/ui/support-link";

export function StartVerificationButton({ start, label, size, className }: { start: StartVerification; label: string; size?: ButtonSize; className?: string }) {
  return (
    <Button type="button" size={size} onClick={start.begin} disabled={start.starting} aria-busy={start.starting} className={className}>
      {start.starting && <Spinner aria-hidden />}
      {label}
    </Button>
  );
}

/**
 * `className` lands on the message, not on the region: the region is mounted empty and stays
 * mounted — a `role="status"` inserted in the same tick as its own text is unreliably
 * announced — so it must carry no spacing of its own, or an empty one would take up space.
 * Each presentation gives the message the margin or alignment its frame needs.
 */
export function VerificationOutcome({ start, className }: { start: StartVerification; className?: string }) {
  return (
    <div role="status">
      <Outcome state={start.state} className={className} />
    </div>
  );
}

function Outcome({ state, className }: { state: StartState; className?: string }) {
  const t = useT();
  // The vendor's actual trouble — no balance, no configuration, an outage — is ours to fix
  // and never reaches the browser, so this says only that it cannot run and hands over the
  // one path that still works: support raising the tier by hand. The address is its own
  // link rather than interpolated into the sentence: `Translate` returns a string, so a link
  // cannot be handed to it as a value.
  //
  // The link is unconditional. The plane's own contact is preferred, but it used to be the
  // ONLY source, and a 503 that omitted the field left the reader with "unavailable" and
  // nowhere to go — the one outcome this whole surface exists to prevent. Today that is not
  // an edge case: verification is deployed unconfigured, so `unavailable` is the outcome
  // nearly every reader gets (see `@/shared/config/support`).
  if (state.kind === "unavailable") {
    return (
      <Note className={className}>
        {t("profile.kyc.unavailable", "Automatic verification is temporarily out of service — write to us and we'll raise your level by hand.")} <SupportLink contact={state.contact ?? undefined} />
      </Note>
    );
  }
  // Not the user's doing and not ours to fix in this session — stated calmly, like the 503.
  if (state.kind === "emailUnverified") return <Note className={className}>{t("profile.kyc.emailUnverified", "Verify your email first — Settings → Security.")}</Note>;
  if (state.kind === "throttled") return <Note className={className}>{t("profile.kyc.tooManyAttempts", "You've already started verification several times today. Please try again tomorrow.")}</Note>;
  if (state.kind === "stale") return <Note className={className} tone="destructive">{t("err.csrf", "This page went stale. Reload it and try again.")}</Note>;
  if (state.kind === "failed") return <Note className={className} tone="destructive">{errorMessage(state.error, t)}</Note>;
  return null;
}

function Note({ tone, className, children }: { tone?: "destructive"; className?: string; children: ReactNode }) {
  return (
    <p className={cn("text-xs leading-snug", className, tone === "destructive" ? "text-accent-error" : "text-ink-soft")}>
      {children}
    </p>
  );
}
