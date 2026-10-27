import { useStepup } from "@evinvest/concierge/react";

import { useEffect, useRef, useState } from "react";

import { intentDeadlineMs, isPastDeadline, needsNewIntent, nowMs, type ConfirmedIntent, type Intent, type IntentError, type IntentKind, type IntentRequestOf, type IntentSummary } from "../lib/intent";
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

export type IntentFlow<K extends IntentKind> =
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
  | { phase: "done"; confirmed: ConfirmedIntent<K> };

/** `proof_late`: a proof came back after its intent's deadline; `previous` is what it was for. */
type Cause = { kind: "person" } | { kind: "countdown" } | { kind: "terms_changed" } | { kind: "proof_late"; previous: IntentSummary };

const sameTerms = (a: IntentSummary, b: IntentSummary) => JSON.stringify(a) === JSON.stringify(b);

/**
 * One money action, from typed inputs to the BFF's answer. The inputs are kept so an expired
 * or changed intent is re-created for the same request, never re-typed. The countdown renews
 * an intent once by itself; after that the person asks for new terms, so an idle page does
 * not mint intents against the per-user money budget forever. An intent with a `min_aal` is
 * confirmed only with a proof from concierge's step-up element, for that intent's hash.
 * `onDone` hears the hub's answer once, when the confirm lands.
 */
export function useIntentFlow<K extends IntentKind>(client: IntentClient, onDone: (confirmed: ConfirmedIntent<K>) => void) {
  const stepup = useStepup();
  // The answer lands after awaits; it goes to the page's handler as it is then.
  const done = useRef(onDone);
  useEffect(() => {
    done.current = onDone;
  });
  const [flow, setFlow] = useState<IntentFlow<K>>({ phase: "editing", error: null });
  const request = useRef<IntentRequestOf<K> | null>(null);
  // The last inputs, rendered back into the form when the person goes back to change them.
  const [draft, setDraft] = useState<IntentRequestOf<K> | null>(null);
  const busy = useRef(false);
  const generation = useRef(0);
  const mounted = useRef(true);
  // The step-up dialog is open for the intent on screen: that intent must not be swapped (no
  // edit, no renewal) until it answers, or its proof would land on terms nobody sees.
  const [stepping, setSteppingState] = useState(false);
  const steppingNow = useRef(false);
  // A proof arrives after awaits; it is judged against the flow as it is then.
  const latest = useRef(flow);

  useEffect(() => {
    latest.current = flow;
  }, [flow]);

  useEffect(() => {
    const counter = generation;
    mounted.current = true;
    return () => {
      mounted.current = false;
      counter.current++;
    };
  }, []);

  const setStepping = (on: boolean) => {
    steppingNow.current = on;
    if (mounted.current) setSteppingState(on);
  };

  // Plain functions, not memoised: they call one another, and every one reads state through refs.
  // `proof` lives in `send`'s call only: it reaches the client and is never put in state. A
  // resend of an unclear confirm goes without one; the BFF resends an executing act unasked.
  async function send(intent: Intent, review: Review | null, proof?: string) {
    if (!mounted.current || busy.current) return;
    if (review && (intent.minAal !== null) !== (proof !== undefined)) throw new Error("a proof goes with exactly the intents that ask for one");
    busy.current = true;
    const mine = ++generation.current;
    if (review) setFlow({ ...review, confirming: true, error: null });
    const result = await client.confirm<K>(intent.id, proof);
    busy.current = false;
    if (mine !== generation.current) return;
    if (result.ok) {
      setFlow({ phase: "done", confirmed: result.value });
      return done.current(result.value);
    }
    const { error } = result;
    if (error.code === "intent_executing" || error.code === "network") return setFlow({ phase: "unclear", intent, askAgain: true });
    // Never after an unclear send: the first act may have run, and a new intent is a new act.
    if (review && needsNewIntent(error)) return void create(error.code === "terms_changed" ? { kind: "terms_changed" } : { kind: "person" });
    setFlow(review ? { ...review, confirming: false, error } : { phase: "unclear", intent, askAgain: false });
  }

  async function prove(review: Review) {
    const { intent } = review;
    if (intent.minAal === null) return void send(intent, review);
    setStepping(true);
    const proof = await stepup.requestProof(intent.hash, intent.minAal);
    setStepping(false);
    if (proof === null || !mounted.current) return;
    const now = latest.current;
    if (now.phase !== "review" || now.intent.id !== intent.id) throw new Error("the intent changed under an open step-up");
    // A first factor set up inside the dialog can take longer than the intent lives.
    if (isPastDeadline(now.deadlineMs, nowMs())) return void create({ kind: "proof_late", previous: intent.summary });
    void send(intent, now, proof);
  }

  async function create(cause: Cause) {
    const sent = request.current;
    if (sent === null || busy.current) return;
    busy.current = true;
    const mine = ++generation.current;
    setFlow({ phase: "creating" });
    const result = await client.create(sent);
    busy.current = false;
    if (mine !== generation.current) return;
    if (!result.ok) return setFlow({ phase: "editing", error: result.error });
    const late = cause.kind === "proof_late";
    const review: Review = {
      phase: "review",
      intent: result.value,
      deadlineMs: intentDeadlineMs(result.value.expiresAt, nowMs()),
      autoRenewed: cause.kind === "countdown",
      termsChanged: cause.kind === "terms_changed" || (late && !sameTerms(cause.previous, result.value.summary)),
      confirming: false,
      error: null,
    };
    latest.current = review;
    setFlow(review);
    // The person already confirmed these terms; only the proof is for a new intent.
    if (late && !review.termsChanged) void prove(review);
  }

  const submit = (next: IntentRequestOf<K>) => {
    request.current = next;
    setDraft(next);
    void create({ kind: "person" });
  };

  const confirm = () => {
    if (flow.phase === "review" && !flow.confirming && !steppingNow.current) void prove(flow);
    else if (flow.phase === "unclear" && flow.askAgain) void send(flow.intent, null);
  };

  const expire = () => {
    if (flow.phase !== "review" || flow.confirming || steppingNow.current) return;
    if (flow.autoRenewed) setFlow({ phase: "expired" });
    else void create({ kind: "countdown" });
  };

  const renew = () => void create({ kind: "person" });

  const edit = () => {
    if (steppingNow.current) return;
    generation.current++;
    busy.current = false;
    setFlow({ phase: "editing", error: null });
  };

  return { flow, draft, stepping, stepupDialog: stepup.dialogProps, submit, confirm, expire, renew, edit };
}

export type IntentFlowControls<K extends IntentKind> = ReturnType<typeof useIntentFlow<K>>;
