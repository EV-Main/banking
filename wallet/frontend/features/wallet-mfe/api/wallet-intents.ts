import { apiPath } from "@/shared/config/base-path";

import type { IntentKind } from "../lib/intent";
import { createIntentClient, type IntentClient } from "../model/intent-client";
import { createStepupClient, type StepupClient } from "../model/stepup-client";
import { renewSession } from "./wallet-remote";

// One client per wallet origin, so every form on the page shares one CSRF token.
const clients = new Map<string, IntentClient>();
const stepupClients = new Map<string, StepupClient>();

export function intentClientFor(origin: string): IntentClient {
  const known = clients.get(origin);
  if (known) return known;
  const client = createIntentClient({
    apiBase: new URL(apiPath("/api/wallet"), origin).href,
    fetch: (input, init) => fetch(input, init),
    renewSession: () => renewSession(origin),
  });
  clients.set(origin, client);
  return client;
}

/** The kinds the wallet origin's BFF runs in this page; none on any failure. */
export function enabledKinds(origin: string): Promise<IntentKind[]> {
  return intentClientFor(origin).kinds();
}

/**
 * Concierge's step-up routes on the wallet origin. The shell's surface, not the zone's: no
 * `/cabinet` prefix, like the session read in `./wallet-remote.ts`.
 */
export function stepupClientFor(origin: string): StepupClient {
  const known = stepupClients.get(origin);
  if (known) return known;
  const client = createStepupClient({
    apiBase: new URL("/api/auth/stepup", origin).href,
    fetch: (input, init) => fetch(input, init),
    renewSession: () => renewSession(origin),
  });
  stepupClients.set(origin, client);
  return client;
}
