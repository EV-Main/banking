// The wire contract of the identity plane's two KYC routes, as the browser sees it.
//
// `/kyc/start` and `/kyc/status` are not in `contracts/openapi.json` — they are web-surface
// routes on concierge, not gRPC, so no generated type exists and none is coming. What the
// slice used to do instead was hand-probe each answer at the point of use: `readString(data,
// "redirect_url")` in one place, `readString(error.body, "contact")` in another. A rename on
// the plane's side turned into "please try again" in front of a healthy provider.
//
// So the shape is written down HERE, once, as narrowing functions. Their counterpart is the
// integration test in concierge (`runner/tests/kyc.rs`), which compares whole JSON documents
// against the same shapes — a rename fails there, and is refused here.
//
// No schema library: four shapes do not earn a dependency in `package.json`, and the probing
// they need is three lines each (see `../lib/read-field`).
//
// The status half has no reader on THIS branch and is not meant to: it is the contract the
// running-case gate is built on one PR up the stack (#190), which is where `KycStatus`,
// `KycCase` and `RUNNING_CASE_STATUSES` acquire their consumer. Pinned here, with the start
// half, because both routes are one release of the plane and drift on either is the same bug.

import { readBoolean, readField, readNumber, readString } from "../lib/read-field.ts";

/**
 * The closed dictionary of `{ "error": … }` codes both routes WILL publish, once
 * concierge#76 (`fen/kyc-start-contract`) ships. Anything outside it is drift, and is read
 * as "no code at all" rather than guessed at.
 *
 * Against the concierge that is deployed today only `kyc_unavailable` is on the wire: every
 * other refusal is still plain text with no JSON body (`StartError::Plain`), and
 * `/kyc/status` does not exist at all. So the status-only fallback in `./start-outcome`
 * is not a legacy branch — it is the branch that fires, and it stays until #76 is released.
 *
 * `kyc_unavailable` is the only one carrying a second field (`contact`), and the only one
 * whose meaning is a designed state rather than a fault.
 */
export const KYC_ERROR_CODES = ["unauthenticated", "csrf", "throttled", "internal", "kyc_unavailable", "email_unverified"] as const;

export type KycErrorCode = (typeof KYC_ERROR_CODES)[number];

export interface KycErrorBody {
  error: KycErrorCode;
  /** Present only on `kyc_unavailable`; `null` on every other code and when omitted. */
  contact: string | null;
}

export interface KycStartResponse {
  redirectUrl: string;
  caseId: string | null;
}

/**
 * A verification attempt that is still running. A decided case is history and never appears
 * here — the plane drops it from the answer the moment it resolves.
 */
export interface KycCase {
  /**
   * The plane's own status word. Kept as an open string on purpose: today it is one of
   * {@link RUNNING_CASE_STATUSES}, but the column is a persistent dictionary that may grow a
   * value before this cabinet ships again. Rejecting an unrecognised word would make the
   * whole status unreadable, and an unreadable status falls back to offering Start — i.e. a
   * new running state would buy a second paid vendor session. A word we do not recognise
   * still means a case is running, which is the only thing the gate needs.
   */
  status: string;
  /** Always the entry tier (1) today; carried rather than assumed. */
  requestedTier: number;
  /** Unix SECONDS, not milliseconds — pinned by the concierge test. */
  createdAt: number;
  /**
   * Whether the vendor session behind this case can still be re-entered. `false` means the
   * case is alive and holds the start gate, but there is no link to send the user back to:
   * the cabinet must offer a fresh start, never a dead link.
   */
  resumable: boolean;
}

export interface KycStatus {
  level: number;
  case: KycCase | null;
}

/** The statuses a running case can carry today. Informational — see {@link KycCase.status}. */
export const RUNNING_CASE_STATUSES = ["pending", "in_progress", "in_review", "resubmitted"] as const;

/** `null` when the body is not a usable start answer — a missing or empty `redirect_url`. */
export function parseStartResponse(body: unknown): KycStartResponse | null {
  const redirectUrl = readString(body, "redirect_url");
  if (redirectUrl === null) return null;
  return { redirectUrl, caseId: readString(body, "case_id") };
}

/**
 * `null` when the body is not a status document. Callers treat that exactly as they treat a
 * 404 from a plane that has not shipped the route yet: fall back to the tier alone.
 */
export function parseStatus(body: unknown): KycStatus | null {
  const level = readNumber(body, "level");
  if (level === null) return null;
  // Three answers, not two. `case` absent is not a status document (an `Option::None` the
  // plane started skipping would otherwise read as an affirmative "nothing running" and
  // silently restore the pre-#190 behaviour); `case: null` IS that affirmative answer; and a
  // `case` holding anything that is not an object — a number, a string, an array — is drift,
  // which takes the whole document down with it rather than passing as "no attempt running"
  // and re-opening the start gate.
  const raw = readField(body, "case");
  if (raw === undefined) return null;
  if (raw === null) return { level, case: null };
  const kycCase = parseCase(raw);
  return kycCase === null ? null : { level, case: kycCase };
}

function parseCase(raw: unknown): KycCase | null {
  const status = readString(raw, "status");
  const requestedTier = readNumber(raw, "requested_tier");
  const createdAt = readNumber(raw, "created_at");
  const resumable = readBoolean(raw, "resumable");
  if (status === null || requestedTier === null || createdAt === null || resumable === null) return null;
  return { status, requestedTier, createdAt, resumable };
}

/**
 * `null` when the body carries no code from {@link KYC_ERROR_CODES}.
 *
 * Keyed on the code and not on the status, because once concierge#76 ships every refusal
 * carries one and the status is corroboration rather than evidence. Until then most refusals
 * arrive bodyless and land in the status-only fallback instead — see the note on
 * {@link KYC_ERROR_CODES}.
 *
 * `contact` is checked for the shape of an address, not merely for being a non-empty string.
 * It is the one field of these bodies that the cabinet puts back into a URL — the support
 * `mailto:` — and `real@support.tld?cc=…&body=…` there turns "write to support" into a
 * pre-addressed letter to someone else. An address that does not look like one degrades to
 * `null`, which every reader already handles by falling back to `SUPPORT_EMAIL`.
 */
export function parseErrorBody(body: unknown): KycErrorBody | null {
  const error = readString(body, "error");
  if (error === null || !isKycErrorCode(error)) return null;
  return { error, contact: error === "kyc_unavailable" ? readContact(body) : null };
}

/** Deliberately cruder than RFC 5322: it exists to exclude URL syntax, not to validate mail. */
const CONTACT = /^[^\s@?#&/\\]+@[^\s@?#&/\\]+\.[^\s@?#&/\\]+$/;

function readContact(body: unknown): string | null {
  const contact = readString(body, "contact");
  return contact !== null && CONTACT.test(contact) ? contact : null;
}

function isKycErrorCode(value: string): value is KycErrorCode {
  return (KYC_ERROR_CODES as readonly string[]).includes(value);
}
