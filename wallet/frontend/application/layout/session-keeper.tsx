"use client";

// Keeps the signed-in shell signed in. Renders nothing.
//
// The shared access cookie carries a short-TTL JWT; the cookie the zone proxy gates on
// outlives it by days (see `shared/lib/session.ts`). Without this, a tab left open — or
// re-entered — past the access TTL renders the page and then fails every `/api/*` call
// with `unauthenticated` until something happens to hit the shell session endpoint. So
// this rotates the cookie on the cadence the TTL needs, and again whenever the tab comes
// back to the user (a hidden tab's timers are throttled or frozen, so returning to it is
// exactly when the cookie is most likely lapsed).
//
// A session the shell calls gone navigates nowhere: the reader stays on the page as a
// guest, and the route gate walls off what their seat no longer holds.

import { useEffect } from "react";

import { STALE_MS, refreshIfStale } from "@/shared/lib/session";

export function SessionKeeper() {
  useEffect(() => {
    const rotate = () => {
      if (document.visibilityState === "visible") void refreshIfStale();
    };
    rotate();
    const timer = setInterval(rotate, STALE_MS);
    document.addEventListener("visibilitychange", rotate);
    window.addEventListener("focus", rotate);
    window.addEventListener("online", rotate);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", rotate);
      window.removeEventListener("focus", rotate);
      window.removeEventListener("online", rotate);
    };
  }, []);

  return null;
}
