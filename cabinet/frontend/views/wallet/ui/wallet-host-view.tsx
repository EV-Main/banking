"use client";

import { Skeleton } from "@evinvest/uikit";

import { WalletPanel, walletActionHref, type WalletAction } from "@/features/wallet-mfe";
import { cn } from "@/shared/lib/cn";
import { useHydrated } from "@/shared/lib/use-hydrated";
import { PAGE_CONTENT_WIDTH, PAGE_PAD } from "@/shared/ui/page-frame";

// The panel needs an absolute origin to build its BFF and action URLs, and only the browser
// knows the public one behind the conductor, so the server render holds a placeholder.
export function WalletHostView() {
  const hydrated = useHydrated();
  return (
    <div className={cn(PAGE_PAD, PAGE_CONTENT_WIDTH)}>
      {hydrated ? <WalletPanel origin={window.location.origin} scope={null} onAction={openAction} /> : <Skeleton className="h-64 w-full rounded-xl" />}
    </div>
  );
}

// The element's own fallback when no host takes the action (`mfe/wallet/entry.tsx`): on
// evinvest.ltd an action runs in place, and the action page honours `return_to` only on
// its own origin.
function openAction(action: WalletAction) {
  const url = new URL(walletActionHref(window.location.origin, action));
  url.searchParams.set("return_to", window.location.origin + window.location.pathname);
  window.location.assign(url.href);
}
