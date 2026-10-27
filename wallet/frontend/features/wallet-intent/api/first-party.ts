import { apiPath } from "@/shared/config/base-path";
import { refreshSession } from "@/shared/lib/session";

import { createIntentClient } from "../model/intent-client";

/** The wallet's own pages: same origin, and the shell's session refresh. */
// Pure: the wallet element's bundle imports this feature and must drop the shell's session.
export const firstPartyIntentClient = /* @__PURE__ */ createIntentClient({
  apiBase: apiPath("/api/wallet"),
  fetch: (input, init) => fetch(input, init),
  // Null is a session whose fate is unknown, not a sign-out.
  renewSession: async () => (await refreshSession())?.authenticated !== false,
});
