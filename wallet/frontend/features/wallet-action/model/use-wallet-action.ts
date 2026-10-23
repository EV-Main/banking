"use client";

import { useRef, useState, useSyncExternalStore } from "react";

import { WALLET_SCOPE_HOSTS } from "@/shared/config/wallet-scopes";

import { readActionParams, resultMessage, type ActionOutcome, type ActionParams } from "../lib/action-params";

export interface FinishedAction {
  outcome: ActionOutcome;
  /** The opener heard the result. */
  reported: boolean;
}

const noSubscribe = () => () => {};
const NO_SEARCH = "";

/**
 * The page's side of the action contract: the checked URL parameters, and `finish`, which
 * reports the outcome to the opener — `postMessage(msg, <opener origin>)`, never `'*'` — at
 * most once per page load. A second outcome (a double click, a cancel after a success) is
 * dropped: the opener counts the first one and money may already have moved.
 */
export function useWalletAction(): { params: ActionParams; finished: FinishedAction | null; finish: (outcome: ActionOutcome, reference?: string | null) => void } {
  // The URL is read on the client only: the server render has no `window.location` and a
  // page that posts a result must agree with what the browser actually loaded.
  const search = useSyncExternalStore(noSubscribe, () => window.location.search, () => NO_SEARCH);
  const origin = useSyncExternalStore(noSubscribe, () => window.location.origin, () => NO_SEARCH);
  const params = readActionParams(search, origin, WALLET_SCOPE_HOSTS);
  const [finished, setFinished] = useState<FinishedAction | null>(null);
  // A ref, not the state: two clicks in one tick both see the state still null.
  const sent = useRef(false);

  const finish = (outcome: ActionOutcome, reference: string | null = null) => {
    if (sent.current) return;
    sent.current = true;
    const message = resultMessage(params, outcome, reference);
    // lib.dom types `opener` as `any`; the target origin below pins who may read the message.
    const opener: Window | null = window.opener;
    let reported = false;
    if (message !== null && params.opener !== null && opener !== null) {
      try {
        opener.postMessage(message, params.opener);
        reported = true;
      } catch {
        // A severed opener (COOP, the host navigated away): the host re-reads its backend.
      }
    }
    setFinished({ outcome, reported });
  };

  return { params, finished, finish };
}
