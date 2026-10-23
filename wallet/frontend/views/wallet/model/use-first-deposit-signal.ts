"use client";

// `first_deposit`, recorded when a wallet screen first shows the account's one credited
// deposit. Mounted by the wallet screens (overview, deposit, activity) — the places a
// reader waits for a credit to land — so the event follows the sighting rather than the
// credit itself: the hub announces nothing to the browser, and a credit lands whenever the
// chain watcher confirms it. That lag is the accepted limit of a client-side funnel; the
// issue names a BFF-side event as the later, exact alternative.
//
// Keyed by the tx reference per browser: the same account never repeats it on another
// visit, and the next account on the same machine has a different first deposit.

import { useAnalytics } from "@evinvest/analytics/react";
import { useEffect } from "react";

import { soleDeposit } from "@/entities/wallet/lib/first-deposit";
import { depositsResource } from "@/entities/wallet/model/wallet-resource";
import { ACTIVATION, once } from "@/shared/analytics";
import { useResource } from "@/shared/lib/resource";

export function useFirstDepositSignal(): void {
  const { data } = useResource(depositsResource);
  const capture = useAnalytics();
  useEffect(() => {
    const first = soleDeposit(data);
    if (first === null) return;
    if (once(`${ACTIVATION.firstDeposit}:${first.tx_ref}`, "browser")) capture(ACTIVATION.firstDeposit, { network: first.network ?? "" });
  }, [data, capture]);
}
