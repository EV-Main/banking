"use client";

// "The role you are holding came from the environment, not from the register."
//
// This exists because of the bug it names, not because a screen wanted a banner. The admin
// console drew "Владелец" for a subject the consilium counted as nobody: one surface read a
// per-request effective role, the other read the stored roster, and neither said which it
// was. Two surfaces disagreeing about who owns the fund with neither admitting it is how an
// operator comes to trust a seat that does not exist (docs/CONSILIUM.md, § Genesis —
// "the original bug was never that the elevation existed, it was that it was invisible").
//
// The fix is not that the elevation went away. It still exists, and it has to: someone must
// be able to reach the console of a system that has no owners yet. What changed is that it
// now names itself — `role_is_break_glass` arrives on the caller's own session, straight
// from concierge, and this is the component that says it out loud. Nothing here is derived,
// cross-referenced or inferred; the predecessor to this component was a mark computed by
// comparing the user list against the owner roster, and it was deleted precisely because a
// client-side derivation can disagree with both of the things it derives from.
//
// One component for both screens on purpose. The admin console and the owners' room are the
// two surfaces where a role decides something, so they are the two that must say this — and
// a sentence about who controls the fund, maintained in two copies, is a sentence that will
// eventually exist in two versions.
//
// Tone is the other half of the design. This is the expected state of a fresh install, not
// an incident, so it takes the `default` Alert and the accent tokens the cabinet already
// uses for "standing fact that changes what you can do" (the payout-floor notice on the
// consilium page is the sibling). `destructive` would read as something to escalate, and
// the one repair it would push someone toward — granting the role to close the gap — is the
// move the whole mechanism exists to prevent.

import { KeyRound } from "lucide-react";

import { useT } from "@evinvest/i18n/react";
import { Alert, AlertDescription, AlertTitle } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";
import { useSession } from "@/shared/lib/use-session";

/**
 * The banner, or nothing at all.
 *
 * Renders itself out of existence unless the caller's own session says the role is
 * break-glass, so a screen mounts it unconditionally and never has to hold the condition.
 * `?? false` covers the session still being in flight and an older shell that predates the
 * field: absent is not "yes", and a banner accusing a legitimate admin of holding borrowed
 * authority would be its own version of the lie this closes.
 *
 * The window it reports is self-limiting — the flag can only be true while the fund has no
 * persisted owner, and the first genesis seeding ends that permanently — so there is no
 * dismissal and nothing to remember. It disappears when the state does.
 */
export function BreakGlassNotice({ className }: { className?: string }) {
  const t = useT();
  const session = useSession();
  if (!(session?.user?.roleIsBreakGlass ?? false)) return null;

  return (
    <Alert className={cn("border-accent-warn/40 bg-accent-warn/10", className)}>
      <KeyRound className="size-4 text-accent-warn" />
      <AlertTitle>{t("session.breakGlass.title")}</AlertTitle>
      <AlertDescription className="text-ink">{t("session.breakGlass.body")}</AlertDescription>
    </Alert>
  );
}
