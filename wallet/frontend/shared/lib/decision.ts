// Reading a settled answer off the wire.
//
// This exists because "has this seat answered yet?" is not the question "is `decision`
// null?", and getting the two confused broke the feature completely. The planes model an
// unanswered seat as an explicit `PENDING` state, and the BFF serialises that enum by name —
// so the field arrives as the string `"pending"`, and an absent one arrives as `""` rather
// than as JSON null. Both are truthy. A view that treats the field as nullable therefore
// reads *every fresh invitation* as already decided.
//
// So the rule is inverted here, once, for every caller: an answer counts as settled only if
// it is one of the words that means an answer. Everything else — "pending", "", a state a
// plane adds next year — is "not yet", which is the safe direction: the worst case is a form
// shown for a request the server will refuse, which produces a visible error, rather than a
// form silently withheld from someone entitled to use it.

const normalise = (raw: unknown): string => (typeof raw === "string" ? raw.trim().toLowerCase() : "");

/** An investor's answer to a payment out of their own claim, once one has been given. */
export type SettledConsent = "approve" | "reject";

export function settledConsent(raw: unknown): SettledConsent | null {
  const value = normalise(raw);
  if (value === "approve" || value === "approved") return "approve";
  if (value === "reject" || value === "rejected") return "reject";
  return null;
}
