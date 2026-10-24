// Concierge's step-up routes (concierge `docs/STEPUP-PROOF.md`), read at the edge, and the
// rules for which method this page offers. The proof these routes answer authorises one
// stored intent; it is handed to the confirm and to nothing else, so no parser here keeps it
// in an error or a log line.
//
// Pure and import-free so `node --test` runs it as is.

import type { IntentKind, IntentSummary, StepupMethod } from "./intent.ts";

/** The WebAuthn request options exactly as the server sent them; the server fixes `rpId`. */
export interface PasskeyOptions {
  publicKey: Record<string, unknown>;
}

export type StepupCeremony =
  | { method: "passkey"; ceremonyId: string; options: PasskeyOptions }
  | { method: "email_code"; ceremonyId: string };

/** Every refusal the step-up routes name, as the page reads them. */
export type StepupError =
  | { code: "no_passkey" }
  | { code: "throttled" }
  /** The ceremony or the code ran out, or the verify reached another replica. */
  | { code: "expired" }
  | { code: "passkey_rejected" }
  | { code: "invalid_code" }
  | { code: "attempts_exceeded" }
  | { code: "captcha" }
  /** The person closed the passkey prompt, or it timed out; the same ceremony may be retried. */
  | { code: "cancelled" }
  /** The browser could not ask the authenticator at all. */
  | { code: "authenticator_failed" }
  | { code: "signed_out" }
  /** The routes are off for this page, or refused how it asked: the popup still works. */
  | { code: "unavailable" }
  | { code: "network" }
  | { code: "malformed" };

export type StepupErrorCode = StepupError["code"];

export type StepupResult<T> = { ok: true; value: T } | { ok: false; error: StepupError };

const CEREMONY_ID = /^[A-Za-z0-9_-]{1,128}$/;
// Three base64url segments: the shape of a JWT, and nothing a header or a URL could break on.
const PROOF = /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/;
const MAX_PROOF_CHARS = 4096;

function field(source: unknown, key: string): unknown {
  return typeof source === "object" && source !== null && !Array.isArray(source) ? Reflect.get(source, key) : undefined;
}

function isRecord(raw: unknown): raw is Record<string, unknown> {
  return typeof raw === "object" && raw !== null && !Array.isArray(raw);
}

function hasCredentialIds(raw: unknown): boolean {
  return raw === undefined || (Array.isArray(raw) && raw.every((credential) => typeof field(credential, "id") === "string"));
}

/** `POST /api/auth/stepup/options` → the ceremony for `method`, or null when the body is not one. */
export function parseCeremony(body: unknown, method: StepupMethod): StepupCeremony | null {
  const ceremonyId = field(body, "ceremony_id");
  if (typeof ceremonyId !== "string" || !CEREMONY_ID.test(ceremonyId) || field(body, "method") !== method) return null;
  if (method === "email_code") return { method, ceremonyId };
  const publicKey = field(field(body, "options"), "publicKey");
  if (!isRecord(publicKey) || typeof publicKey.challenge !== "string" || !hasCredentialIds(publicKey.allowCredentials)) return null;
  return { method, ceremonyId, options: { publicKey } };
}

/** `POST /api/auth/stepup/verify` → the proof, or null. The proof is opaque to the page. */
export function parseProof(body: unknown): string | null {
  const proof = field(body, "proof");
  return typeof proof === "string" && proof.length <= MAX_PROOF_CHARS && PROOF.test(proof) ? proof : null;
}

const NAMED: Readonly<Record<string, StepupError>> = {
  no_passkey: { code: "no_passkey" },
  throttled: { code: "throttled" },
  passkey_expired: { code: "expired" },
  code_expired: { code: "expired" },
  passkey_rejected: { code: "passkey_rejected" },
  invalid_code: { code: "invalid_code" },
  attempts_exceeded: { code: "attempts_exceeded" },
  captcha: { code: "captcha" },
};

/** A non-2xx answer as the error it names; the page's own refusals read as `unavailable`. */
export function stepupErrorOf(status: number, body: unknown): StepupError {
  const named = field(body, "error");
  if (typeof named === "string" && Object.hasOwn(NAMED, named)) return NAMED[named];
  if (status === 429) return { code: "throttled" };
  return { code: "unavailable" };
}

/** A failed `navigator.credentials.get`, by the DOMException name the browser gave it. */
export function assertionErrorOf(name: string): StepupError {
  return name === "NotAllowedError" || name === "AbortError" ? { code: "cancelled" } : { code: "authenticator_failed" };
}

/** Whether a step-up error is final for this intent: the way on is a new one. */
export function needsFreshIntent(error: StepupError): boolean {
  return error.code === "expired" || error.code === "throttled" || error.code === "no_passkey" || error.code === "attempts_exceeded";
}

// The doc's rule: a withdrawal or a mandate grant takes a passkey only, whatever is offered.
const EMAIL_KINDS: readonly IntentKind[] = ["move", "pay"];

export interface PageAbilities {
  /** The browser has WebAuthn at all. */
  passkey: boolean;
  /** The emailed code can be asked from this page: a Turnstile key and the host's CSP for it. */
  emailCode: boolean;
}

/**
 * The methods this page offers for an intent, in the BFF's order: only methods the BFF listed,
 * only the ones the page can carry out, and never an emailed code for a kind that takes a
 * passkey only. Empty means the popup on EV Invest. There is no password method to pick.
 */
export function pageMethods(kind: IntentKind, offered: readonly StepupMethod[], can: PageAbilities): StepupMethod[] {
  // Each method is named, so a word the BFF should never send (a password) matches neither.
  return offered.filter((method) => (method === "passkey" && can.passkey) || (method === "email_code" && can.emailCode && EMAIL_KINDS.includes(kind)));
}

const MAX_SUMMARY_CHARS = 240;
// Concierge refuses a summary with a control or a bidi character; a payee's display name is
// the hub's, so it is cleaned here rather than trusted to be plain. C0 and C1 controls, the
// bidi marks and embeddings (U+061C, U+200E-200F, U+202A-202E, U+2066-2069) and the
// zero-width characters (U+200B-200D, U+2060-2065, U+FEFF). Escapes, not literals: the
// literals are invisible in review.
const UNPRINTABLE = /[\u0000-\u001F\u007F-\u009F\u061C\u200B-\u200F\u202A-\u202E\u2060-\u2069\uFEFF]/g;

const VERBS: Readonly<Record<IntentKind, string>> = { move: "Move", withdraw: "Withdraw", pay: "Pay", mandate: "Allow" };

/**
 * The one line an emailed code quotes, built from the BFF's terms only, never from typed input.
 * English, like every mail of that plane. Cut to the route's 240 characters.
 */
export function stepupSummary(kind: IntentKind, summary: IntentSummary): string {
  const parts = [VERBS[kind]];
  if (summary.amount !== null) parts.push(`${summary.amount} USDT`);
  parts.push(`to ${summary.recipient}`);
  if (summary.network !== null) parts.push(`on ${summary.network}`);
  if (summary.net !== null && summary.net !== summary.amount) parts.push(`(recipient gets ${summary.net} USDT)`);
  parts.push(`from ${summary.hostOrigin}`);
  const line = parts.join(" ").replace(UNPRINTABLE, "").replace(/\s+/g, " ").trim();
  // Code points, as concierge counts them: a UTF-16 cut could split a surrogate pair.
  const points = Array.from(line);
  return points.length <= MAX_SUMMARY_CHARS ? line : `${points.slice(0, MAX_SUMMARY_CHARS - 1).join("")}…`;
}
