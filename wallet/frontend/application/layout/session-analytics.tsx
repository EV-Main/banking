"use client";

// Ties product analytics to the signed-in account. Renders nothing.
//
// On the first authenticated render this tab sees, the PostHog person is identified by the
// canonical user id (never the email or a name), and `session_created` is recorded once
// per tab per account — the funnel's "reached the cabinet signed in" step, which the
// landing's own steps lead into. Identification goes first so the landing session and
// everything after it merge into one person; the event follows whether or not the SDK
// came up in time, since the step happened either way.
//
// The keeper's answers drive it, not a one-shot read: a sign-out reaching this tab from
// another one resets the person, so the next account on this machine starts anonymous.
// A session the shell could not confirm (`SESSION_UNAVAILABLE` — offline, 5xx) is neither
// a sign-in nor a sign-out and changes nothing here.

import { useAnalytics } from "@evinvest/analytics/react";
import { useEffect } from "react";

import { ACTIVATION, identity, marked, once } from "@/shared/analytics";
import { SESSION_UNAVAILABLE, useSession } from "@/shared/lib/use-session";

export function SessionAnalytics() {
  const session = useSession();
  const capture = useAnalytics();

  useEffect(() => {
    if (session === null || session === SESSION_UNAVAILABLE) return;
    const userId = session.authenticated ? session.user?.userId : undefined;
    if (!userId) {
      identity.reset();
      return;
    }
    void identity.identify(userId).finally(() => {
      if (!once(`session_created:${userId}`)) return;
      // Whether the sign-in page was seen on this tab first, or the session arrived from
      // the site's own sign-in — a flag, never the path it came from.
      capture(ACTIVATION.sessionCreated, { from_login_view: marked("login_view") });
    });
  }, [session, capture]);

  return null;
}
