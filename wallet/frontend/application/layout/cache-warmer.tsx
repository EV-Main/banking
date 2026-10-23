"use client";

// Warms the balance once a signed-in page is up, and drops every cached value when the
// session ends. Renders nothing.
//
// The clear is the half that matters for correctness: this tab may be handed to a different
// account (the shell answers "not authenticated", the keeper moves to /login), and no figure
// belonging to the previous one may survive that.

import { useEffect } from "react";

import { walletResource } from "@/entities/wallet/model/wallet-resource";
import { clearResources } from "@/shared/lib/resource";
import { onSessionChange } from "@/shared/lib/session";

export function CacheWarmer() {
  useEffect(() => warmAtIdle(), []);

  useEffect(
    () =>
      onSessionChange((session) => {
        if (!session.authenticated) clearResources();
      }),
    [],
  );

  return null;
}

// At idle, so the page's own reads go first: nothing here is needed to paint it.
function warmAtIdle(): () => void {
  const warm = () => walletResource.prefetch();
  if (typeof requestIdleCallback === "function") {
    const handle = requestIdleCallback(warm, { timeout: 2_000 });
    return () => cancelIdleCallback(handle);
  }
  // Safari has no requestIdleCallback; a short timer is close enough for a warm-up.
  const timer = setTimeout(warm, 500);
  return () => clearTimeout(timer);
}
