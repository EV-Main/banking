// Payload-hash display. One rule, because several surfaces show it and they must agree:
// the approval email, the approval page and the owners' room all print the same prefix of
// the same hash, and an owner comparing them is the entire point of showing it at all.

/**
 * How much of the payload hash a reader sees.
 *
 * Enough to compare against the prefix printed in the email — which is the whole job: an
 * owner checks that the page in front of them describes the request they were written to
 * about (docs/CONSILIUM.md, policy 12). Not enough to be mistaken for the full value, which
 * stays server-side and is re-verified against the payload at execution.
 */
const HASH_PREFIX_CHARS = 12;

export function hashPrefix(hash: string | undefined): string {
  const value = (hash ?? "").trim();
  if (!value) return "—";
  return value.length > HASH_PREFIX_CHARS ? `${value.slice(0, HASH_PREFIX_CHARS)}…` : value;
}
