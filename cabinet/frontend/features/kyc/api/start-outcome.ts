// Which refusal of `/kyc/start` is which OUTCOME for a screen.
//
// Split out of `./kyc-client` so it can be run: `kyc-client.ts` imports through the `@/`
// alias, which the test runner (`node --test --experimental-strip-types`) does not resolve,
// so anything living there is untestable by construction — and this is the only part of the
// slice whose behaviour a plane release can change under it.
//
// It takes the status and the body rather than the `RequestError` that carries them, which
// is what keeps it importable: `instanceof RequestError` would drag the whole transport —
// and the session machinery under it — into a unit test that needs neither.

import { readString } from "../lib/read-field.ts";
import { parseErrorBody } from "./kyc-contract.ts";

/**
 * `plain` means "nothing here the transport's own error table does not already word" — the
 * caller re-raises what it caught rather than authoring a second sentence for it.
 */
export type StartRefusal =
  | { kind: "unavailable"; contact: string | null }
  | { kind: "stale" }
  | { kind: "throttled" }
  | { kind: "emailUnverified" }
  | { kind: "plain" };

export function classifyRefusal(status: number, body: unknown): StartRefusal {
  // Keyed on the body's code, not on the status, for every code the plane publishes: a
  // dictionary entry is a decision the plane made, a status is a guess we make about it.
  const refusal = parseErrorBody(body);
  switch (refusal?.error) {
    case "kyc_unavailable":
      return { kind: "unavailable", contact: refusal.contact };
    case "throttled":
      return { kind: "throttled" };
    case "email_unverified":
      return { kind: "emailUnverified" };
    case "csrf":
      return { kind: "stale" };
    // `internal` and `unauthenticated` are both worded by `shared/lib/api-client`'s own
    // table, so they need no outcome of their own here.
    default:
      break;
  }
  // No code we know. The deployed plane answers most refusals in plain text with no JSON
  // body at all (`StartError::Plain`), so this is not a legacy path — it is the path, until
  // concierge#76 ships the dictionary. It says only what the status alone can say.
  //
  // The 403 rule is narrow on purpose: a refusal that NAMES a reason is a refusal on the
  // merits, and telling that reader the page went stale sends them reloading forever instead
  // of to support. Only a bodyless 403 is the plane's plain-text `csrf check failed`.
  if (status === 429) return { kind: "throttled" };
  if (status === 403 && readString(body, "error") === null) return { kind: "stale" };
  return { kind: "plain" };
}
