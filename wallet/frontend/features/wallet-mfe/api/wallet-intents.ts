import { createIntentClient, type IntentClient, type IntentKind } from "@/features/wallet-intent";
import { apiPath } from "@/shared/config/base-path";

import { renewSession } from "./wallet-remote";

// One client per wallet origin, so every form on the page shares one CSRF token.
const clients = new Map<string, IntentClient>();

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
