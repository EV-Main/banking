import { useCallback, useEffect, useRef, useState } from "react";

import type { Intent } from "../lib/intent";
import type { StepupCeremony, StepupError } from "../lib/stepup";
import { assertPasskey } from "./passkey-assertion";
import type { StepupClient } from "./stepup-client";

export type PasskeyStepup = { status: "idle" } | { status: "working" } | { status: "failed"; error: StepupError };

/**
 * The passkey step-up for one intent: open a ceremony, ask the authenticator, close the
 * ceremony, and hand the proof to `onProof` — directly, never through state. One run at a
 * time, so a double click cannot mint two proofs. A prompt the person closed keeps its
 * ceremony (it is spent only by a verify), so "try again" asks the authenticator at once.
 */
export function usePasskeyStepup(
  client: StepupClient,
  intent: Intent,
  onProof: (intentId: string, proof: string) => void,
  onWorking: (working: boolean) => void,
) {
  const [state, setState] = useState<PasskeyStepup>({ status: "idle" });
  const running = useRef(false);
  const held = useRef<{ intentId: string; ceremony: Extract<StepupCeremony, { method: "passkey" }> } | null>(null);
  const live = useRef(true);
  // The latest handler, so a proof that arrives after a re-render meets today's flow.
  const deliver = useRef(onProof);

  useEffect(() => {
    deliver.current = onProof;
  }, [onProof]);

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  const run = useCallback(async () => {
    if (running.current) return;
    running.current = true;
    onWorking(true);
    setState({ status: "working" });
    const settle = (next: PasskeyStepup) => {
      running.current = false;
      onWorking(false);
      if (live.current) setState(next);
    };

    let ceremony = held.current?.intentId === intent.id ? held.current.ceremony : null;
    if (ceremony === null) {
      const begun = await client.begin(intent.hash, { method: "passkey" });
      if (!begun.ok) return settle({ status: "failed", error: begun.error });
      if (begun.value.method !== "passkey") return settle({ status: "failed", error: { code: "malformed" } });
      ceremony = begun.value;
      held.current = { intentId: intent.id, ceremony };
    }
    // Gone while the ceremony opened (the form closed, the element left the page): no prompt.
    if (!live.current) return settle({ status: "idle" });

    const assertion = await assertPasskey(ceremony.options);
    if (!assertion.ok) {
      if (assertion.error.code !== "cancelled") held.current = null;
      return settle({ status: "failed", error: assertion.error });
    }

    held.current = null;
    const finished = await client.finish(ceremony.ceremonyId, { response: assertion.value });
    if (!finished.ok) return settle({ status: "failed", error: finished.error });
    // A proof for a panel that is gone is dropped here, unused; it expires within 120 s.
    if (!live.current) return settle({ status: "idle" });
    settle({ status: "idle" });
    deliver.current(intent.id, finished.value);
  }, [client, intent, onWorking]);

  return { state, run };
}
