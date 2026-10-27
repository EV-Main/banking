// The planes' refusals as they reach the browser: the hub's sentence with the `DomainError`
// Display prefix still on it — "conflict: the owner roster changed…" (domain/src/error.rs) —
// or concierge's bare one, "incorrect code — 4 attempts remain". The BFF
// relays a client-safe status's message verbatim (wallet/backend/src/error.rs), so the
// prefix is not something a screen can ask it to drop.
//
// Two readers of that prose: the facts are read out of the sentence and rendered in the reader's language, and anything unrecognised
// falls through to the caller's existing handling. Matching is anchored on the invariant
// fragment that names the condition, never on the whole string.

/** The `DomainError` variants whose Display the hub prefixes its own sentence with. */
const TRANSPORT_PREFIX = /^\s*(?:validation failed|conflict|precondition failed|forbidden):\s*/i;

/** The hub's sentence without the variant name in front of it, for the places that still
 *  show it verbatim. Prose that never carried one comes back untouched. */
export function stripTransportPrefix(message: string): string {
  return message.replace(TRANSPORT_PREFIX, "");
}

function messageOf(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return "";
}

/**
 * A refused approval code, and how many attempts the server says are left.
 *
 * Concierge counts the attempt in the same transaction as the comparison and names the
 * remainder in its refusal ("incorrect code — N attempts remain"); that figure is the
 * server's, so a screen may show it as the count under the field rather than as prose.
 * `null` for anything else, including a wrong-code sentence whose count did not parse — the
 * screen then re-reads the invitation rather than guessing.
 */
export function wrongCodeAttempts(error: unknown): number | null {
  const message = messageOf(error).toLowerCase();
  if (!message.includes("incorrect code")) return null;
  const found = /(\d+)\s+attempts?\s+remain\b/.exec(message);
  return found ? Number(found[1]) : null;
}
