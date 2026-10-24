import { useCallback, useEffect, useRef, useState } from "react";

import { intentDeadlineMs, needsNewIntent, type ConfirmedIntent, type Intent, type IntentError, type IntentRequest } from "../lib/intent";
import type { IntentClient } from "./intent-client";

export interface Review {
  phase: "review";
  intent: Intent;
  deadlineMs: number;
  /** Minted by the countdown, not by the person: the next expiry stops instead of renewing. */
  autoRenewed: boolean;
  /** The terms moved between create and confirm; these are the new ones. */
  termsChanged: boolean;
  confirming: boolean;
  error: IntentError | null;
}

export type IntentFlow =
  | { phase: "editing"; error: IntentError | null }
  | { phase: "creating" }
  | Review
  | { phase: "expired" }
  /**
   * The confirm was sent and its outcome is unknown. `askAgain`: confirming the same intent
   * again is safe and will tell. Otherwise the BFF no longer answers for it (an intent that
   * ran answers its result once), and only the activity list can say what happened.
   */
  | { phase: "unclear"; intent: Intent; askAgain: boolean }
  | { phase: "done"; confirmed: ConfirmedIntent };

type Cause = "person" | "countdown" | "terms_changed";

/**
 * One money action, from typed inputs to the BFF's answer. The inputs are kept so an expired
 * or changed intent is re-created for the same request, never re-typed. The countdown renews
 * an intent once by itself; after that the person asks for new terms, so an idle page does
 * not mint intents against the per-user money budget forever.
 */
export function useIntentFlow(client: IntentClient) {
  const [flow, setFlow] = useState<IntentFlow>({ phase: "editing", error: null });
  const request = useRef<IntentRequest | null>(null);
  // The last inputs, rendered back into the form when the person goes back to change them.
  const [draft, setDraft] = useState<IntentRequest | null>(null);
  const busy = useRef(false);
  const generation = useRef(0);

  useEffect(() => {
    const counter = generation;
    return () => {
      counter.current++;
    };
  }, []);

  const create = useCallback(
    async (cause: Cause) => {
      const sent = request.current;
      if (sent === null || busy.current) return;
      busy.current = true;
      const mine = ++generation.current;
      setFlow({ phase: "creating" });
      const result = await client.create(sent);
      busy.current = false;
      if (mine !== generation.current) return;
      if (!result.ok) return setFlow({ phase: "editing", error: result.error });
      setFlow({
        phase: "review",
        intent: result.value,
        deadlineMs: intentDeadlineMs(result.value.expiresAt, Date.now()),
        autoRenewed: cause === "countdown",
        termsChanged: cause === "terms_changed",
        confirming: false,
        error: null,
      });
    },
    [client],
  );

  const send = useCallback(
    async (intent: Intent, review: Review | null) => {
      if (busy.current || intent.stepup.required) return;
      busy.current = true;
      const mine = ++generation.current;
      if (review) setFlow({ ...review, confirming: true, error: null });
      const result = await client.confirm(intent.id);
      busy.current = false;
      if (mine !== generation.current) return;
      if (result.ok) return setFlow({ phase: "done", confirmed: result.value });
      const { error } = result;
      if (error.code === "intent_executing" || error.code === "network") return setFlow({ phase: "unclear", intent, askAgain: true });
      // Never after an unclear send: the first act may have run, and a new intent is a new act.
      if (review && needsNewIntent(error)) return void create(error.code === "terms_changed" ? "terms_changed" : "person");
      setFlow(review ? { ...review, confirming: false, error } : { phase: "unclear", intent, askAgain: false });
    },
    [client, create],
  );

  const submit = useCallback(
    (next: IntentRequest) => {
      request.current = next;
      setDraft(next);
      void create("person");
    },
    [create],
  );

  const confirm = useCallback(() => {
    if (flow.phase === "review") void send(flow.intent, flow);
    else if (flow.phase === "unclear" && flow.askAgain) void send(flow.intent, null);
  }, [flow, send]);

  const expire = useCallback(() => {
    if (flow.phase !== "review" || flow.confirming) return;
    if (flow.autoRenewed) setFlow({ phase: "expired" });
    else void create("countdown");
  }, [flow, create]);

  const renew = useCallback(() => void create("person"), [create]);

  const edit = useCallback(() => {
    generation.current++;
    busy.current = false;
    setFlow({ phase: "editing", error: null });
  }, []);

  return { flow, draft, submit, confirm, expire, renew, edit };
}

export type IntentFlowControls = ReturnType<typeof useIntentFlow>;
