import { conciergeEntry, fetchConciergeRegistry } from "@evinvest/concierge";
import type { MfeEntry, ParseResult } from "@evinvest/mfe";

import { useEffect, useState } from "react";

export type StepupSource = { status: "loading" } | { status: "ready"; scriptUrl: string } | { status: "failed" };

// Concierge serves its bundles beside its routes, under `/api/auth/` of the wallet origin: the
// conductor fronts both, in production and in dev, as for the session read the client renews.
const MFE_DIR = "/api/auth/mfe/";

const registries = new Map<string, Promise<ParseResult<MfeEntry[]>>>();

function registry(origin: string): Promise<ParseResult<MfeEntry[]>> {
  const known = registries.get(origin);
  if (known) return known;
  const dir = new URL(MFE_DIR, origin).href;
  const read = fetchConciergeRegistry(new URL("registry.json", dir), { allowedPrefix: dir }).then((result) => {
    // Not kept: the next form that mounts asks again instead of staying without a step-up.
    if (!result.ok) registries.delete(origin);
    return result;
  });
  registries.set(origin, read);
  return read;
}

/** The step-up element's bundle from concierge's registry on `origin`; null is the page's own. */
export function useStepupSource(origin: string | null): StepupSource {
  const [source, setSource] = useState<StepupSource>({ status: "loading" });
  useEffect(() => {
    let live = true;
    void registry(origin ?? window.location.origin).then((read) => {
      if (!live) return;
      const entry = read.ok ? conciergeEntry(read.value, "stepup") : read;
      if (!entry.ok) console.error(`step-up element unavailable: ${entry.error}`);
      setSource(entry.ok ? { status: "ready", scriptUrl: entry.value.scriptUrl } : { status: "failed" });
    });
    return () => {
      live = false;
    };
  }, [origin]);
  return source;
}
