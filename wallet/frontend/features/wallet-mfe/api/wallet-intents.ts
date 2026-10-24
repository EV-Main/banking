import { apiPath } from "@/shared/config/base-path";

import { createIntentClient, type IntentClient } from "../model/intent-client";
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
