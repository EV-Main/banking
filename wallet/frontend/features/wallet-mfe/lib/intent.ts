// The wallet intent contract (docs/WALLET-INTENTS.md), read at the edge. The BFF computes
// every term a person confirms, so the page shows only what this parser accepted and never a
// value it typed itself. A body that does not parse is a malformed answer, not a partial one.
//
// Pure and import-free so `node --test` runs it as is.

export type IntentKind = "move" | "withdraw" | "pay" | "mandate";

export type IntentRequest =
  | { kind: "move"; params: { from_scope: string; to_scope: string; amount: string } }
  | { kind: "withdraw"; params: { network: string; address: string; amount: string } }
  | { kind: "pay"; params: { invoice_id: string; pay_from: string } }
  | { kind: "mandate"; params: { request_id: string } };

export type StepupMethod = "passkey" | "email_code";

export type MandateOperation = "reserve" | "charge" | "book_fill";

export interface MandateCaps {
  clientName: string;
  /** Known operations by name; an operation this page does not know stays its wire name. */
  operations: string[];
  capPerOp: string;
  cap24h: string;
  capOutstanding: string;
  /** Empty when the service asked for no book-fill cap. */
  capBookFill24h: string;
  /** Unix seconds; 0 means until the person ends it. */
  expiresAt: number;
}

export interface IntentSummary {
  /** Decimal USDT; null where the hub states none (a mandate moves nothing now). */
  amount: string | null;
  fee: string | null;
  net: string | null;
  /** The full recipient: an address, a scope, or the payee's name. Never shortened. */
  recipient: string;
  network: string | null;
  scope: string;
  hostOrigin: string;
  mandate: MandateCaps | null;
}

export interface Intent {
  id: string;
  hash: string;
  summary: IntentSummary;
  /** Unix seconds, on the BFF's clock. */
  expiresAt: number;
  stepup: { required: boolean; methods: StepupMethod[] };
}

export interface CsrfToken {
  token: string;
  expiresAt: number;
  /** The kinds this host may create now; the in-page form opens only for these. */
  kinds: IntentKind[];
}

export interface ConfirmedIntent {
  intentId: string;
  /** The hub's id of what it did (the move, the withdrawal), when its answer names one. */
  reference: string | null;
}

/** Every refusal the BFF names in `{ error }` on the intent routes. */
export const INTENT_REFUSALS = [
  "csrf",
  "account_not_active",
  "identity_unavailable",
  "store_unavailable",
  "invalid_request",
  "kind_not_enabled",
  "scope_mismatch",
  "network_unavailable",
  "below_minimum",
  "below_fee",
  "invoice_not_open",
  "mandate_request_expired",
  "intent_not_found",
  "session_changed",
  "intent_not_pending",
  "intent_expired",
  "proof_required",
  "proof_not_expected",
  "terms_changed",
  "stepup_invalid",
  "proof_replayed",
  "email_limit_reached",
  "limit_reached",
  "intent_executing",
] as const;

export type IntentRefusal = (typeof INTENT_REFUSALS)[number];

/** The input a 400 with prose names (`invalid amount`, `invalid address`, …). */
export type InputField = "amount" | "address" | "network" | "id";

export type IntentError =
  | { code: IntentRefusal }
  | { code: "invalid_input"; field: InputField | null }
  /** 401: the session did not verify, and renewing it did not help. */
  | { code: "unauthenticated" }
  /** The shell answered that nobody is signed in. */
  | { code: "signed_out" }
  | { code: "unsupported_media_type" }
  | { code: "rate_limited" }
  /** 404 `not_found`: the intent routes are not mounted for this page. */
  | { code: "not_enabled" }
  /** No answer at all; for a confirm the act may or may not have run. */
  | { code: "network" }
  | { code: "unavailable" }
  /** A 2xx whose body is not the contract. */
  | { code: "malformed" };

export type IntentErrorCode = IntentError["code"];

export type IntentResult<T> = { ok: true; value: T } | { ok: false; error: IntentError };

const INTENT_ID = /^[0-9a-f]{32}$/;
const INTENT_HASH = /^[0-9a-f]{64}$/;
const DECIMAL = /^\d+(?:\.\d+)?$/;
const SCOPE = /^[A-Za-z0-9_-]{1,128}$/;
const CSRF = /^[A-Za-z0-9._-]{1,256}$/;
const INPUT_FIELDS: Readonly<Record<string, InputField>> = {
  "invalid amount": "amount",
  "invalid address": "address",
  "unknown network": "network",
  "invalid id": "id",
};
const OPERATION_PREFIX = "MANDATE_OPERATION_";
const OPERATIONS: readonly string[] = ["reserve", "charge", "book_fill"] satisfies MandateOperation[];

function field(source: unknown, key: string): unknown {
  return typeof source === "object" && source !== null ? Reflect.get(source, key) : undefined;
}

function text(raw: unknown, max: number): string | null {
  return typeof raw === "string" && raw !== "" && raw.length <= max ? raw : null;
}

function decimalOrNull(raw: unknown): string | null | undefined {
  if (raw === null) return null;
  return typeof raw === "string" && DECIMAL.test(raw) ? raw : undefined;
}

function seconds(raw: unknown): number | null {
  return typeof raw === "number" && Number.isSafeInteger(raw) && raw >= 0 ? raw : null;
}

function isRefusal(raw: unknown): raw is IntentRefusal {
  return typeof raw === "string" && INTENT_REFUSALS.some((code) => code === raw);
}

const KINDS: readonly string[] = ["move", "withdraw", "pay", "mandate"] satisfies IntentKind[];

function isKind(raw: unknown): raw is IntentKind {
  return typeof raw === "string" && KINDS.includes(raw);
}

function isMethod(raw: unknown): raw is StepupMethod {
  return raw === "passkey" || raw === "email_code";
}

/** `MANDATE_OPERATION_BOOK_FILL` → `book_fill`; an unknown name stays as the wire wrote it. */
export function mandateOperation(raw: string): string {
  const name = raw.startsWith(OPERATION_PREFIX) ? raw.slice(OPERATION_PREFIX.length).toLowerCase() : raw;
  return OPERATIONS.includes(name) ? name : raw;
}

function parseMandate(raw: unknown): MandateCaps | null | undefined {
  if (raw === null || raw === undefined) return null;
  const operations = field(raw, "operations");
  const caps = ["cap_per_op", "cap_24h", "cap_outstanding"].map((key) => decimalOrNull(field(raw, key)));
  const bookFill = field(raw, "cap_book_fill_24h");
  const expiresAt = seconds(field(raw, "mandate_expires_at"));
  const clientId = text(field(raw, "client_id"), 256);
  const displayName = field(raw, "client_display_name");
  if (!Array.isArray(operations) || !operations.every((op) => typeof op === "string") || expiresAt === null || clientId === null) return undefined;
  const [capPerOp, cap24h, capOutstanding] = caps;
  if (typeof capPerOp !== "string" || typeof cap24h !== "string" || typeof capOutstanding !== "string") return undefined;
  if (typeof bookFill !== "string" || (bookFill !== "" && !DECIMAL.test(bookFill))) return undefined;
  return {
    clientName: text(displayName, 256) ?? clientId,
    operations: operations.map(mandateOperation),
    capPerOp,
    cap24h,
    capOutstanding,
    capBookFill24h: bookFill,
    expiresAt,
  };
}

function parseSummary(raw: unknown): IntentSummary | null {
  const amount = decimalOrNull(field(raw, "amount"));
  const fee = decimalOrNull(field(raw, "fee"));
  const net = decimalOrNull(field(raw, "net"));
  const recipient = text(field(raw, "recipient_full"), 512);
  const networkRaw = field(raw, "network");
  const network = networkRaw === null ? null : text(networkRaw, 32);
  const scope = text(field(raw, "scope"), 128);
  const hostOrigin = text(field(raw, "host_origin"), 256);
  const mandate = parseMandate(field(raw, "mandate"));
  if (amount === undefined || fee === undefined || net === undefined || mandate === undefined) return null;
  if (recipient === null || (networkRaw !== null && network === null) || scope === null || !SCOPE.test(scope) || hostOrigin === null) return null;
  return { amount, fee, net, recipient, network, scope, hostOrigin, mandate };
}

/** `POST /api/wallet/intents` → the intent, or null when the body is not one. */
export function parseIntent(body: unknown): Intent | null {
  const id = field(body, "intent_id");
  const hash = field(body, "intent_hash");
  const expiresAt = seconds(field(body, "expires_at"));
  const summary = parseSummary(field(body, "summary"));
  const stepup = field(body, "stepup");
  const required = field(stepup, "required");
  const methods = field(stepup, "methods");
  if (typeof id !== "string" || !INTENT_ID.test(id) || typeof hash !== "string" || !INTENT_HASH.test(hash)) return null;
  if (expiresAt === null || summary === null || typeof required !== "boolean" || !Array.isArray(methods)) return null;
  return { id, hash, summary, expiresAt, stepup: { required, methods: methods.filter(isMethod) } };
}

/**
 * `GET /api/wallet/csrf` → the token for `x-ev-csrf` and the host's enabled kinds, or null.
 * A kind this bundle does not know is dropped, so a kind the matrix adds later leaves an older
 * bundle on the popup instead of failing it. A BFF that predates `kinds` enables none.
 */
export function parseCsrf(body: unknown): CsrfToken | null {
  const token = field(body, "csrf");
  const expiresAt = seconds(field(body, "expires_at"));
  const kinds = field(body, "kinds");
  if (typeof token !== "string" || !CSRF.test(token) || expiresAt === null) return null;
  return { token, expiresAt, kinds: Array.isArray(kinds) ? [...new Set(kinds.filter(isKind))] : [] };
}

/** `POST /api/wallet/intents/{id}/confirm` → what ran, or null unless it says `executed`. */
export function parseConfirmed(body: unknown): ConfirmedIntent | null {
  const intentId = field(body, "intent_id");
  if (typeof intentId !== "string" || !INTENT_ID.test(intentId) || field(body, "status") !== "executed") return null;
  return { intentId, reference: text(field(field(body, "result"), "id"), 256) };
}

/** A non-2xx answer as the error it names; anything unnamed falls back to its status. */
export function intentErrorOf(status: number, body: unknown): IntentError {
  const named = field(body, "error");
  if (isRefusal(named)) return { code: named };
  if (status === 401) return { code: "unauthenticated" };
  if (status === 415) return { code: "unsupported_media_type" };
  if (status === 429) return { code: "rate_limited" };
  if (status === 404 && named === "not_found") return { code: "not_enabled" };
  if (status === 400) {
    const known = typeof named === "string" && Object.hasOwn(INPUT_FIELDS, named) ? INPUT_FIELDS[named] : null;
    return { code: "invalid_input", field: known };
  }
  return { code: "unavailable" };
}

/** The BFF holds an intent this long; a deadline past it is a clock the page cannot trust. */
export const INTENT_TTL_MS = 120_000;

/**
 * When the page stops offering the confirm, in local milliseconds. `expiresAt` is on the
 * BFF's clock, so the distance to it is capped at the intent's lifetime: a local clock that
 * runs behind cannot show more time than the BFF gives, and one that runs ahead ends early.
 */
export function intentDeadlineMs(expiresAt: number, receivedAtMs: number): number {
  const left = expiresAt * 1000 - receivedAtMs;
  return receivedAtMs + Math.min(Math.max(left, 0), INTENT_TTL_MS);
}

/** Whole seconds left before `deadlineMs`, never below zero. */
export function secondsLeft(deadlineMs: number, nowMs: number): number {
  return Math.max(0, Math.ceil((deadlineMs - nowMs) / 1000));
}

/** `125` → `"2:05"`. */
export function clockText(totalSeconds: number): string {
  const minutes = Math.floor(totalSeconds / 60);
  const rest = totalSeconds % 60;
  return `${minutes}:${String(rest).padStart(2, "0")}`;
}

/** Whether a failed confirm should be sent again for the same intent. */
export function confirmAgain(error: IntentError): boolean {
  return error.code === "intent_executing" || error.code === "network";
}

/**
 * Whether confirming this intent again cannot help: its proof was spent or refused, or a 24 h
 * line was reached (the BFF cancels the intent for those). The way on is a new intent.
 */
export function intentCancelled(error: IntentError): boolean {
  return error.code === "proof_replayed" || error.code === "stepup_invalid" || error.code === "limit_reached" || error.code === "email_limit_reached";
}

/** Whether the person has to start from a new intent, with the same inputs. */
export function needsNewIntent(error: IntentError): boolean {
  return error.code === "intent_expired" || error.code === "terms_changed" || error.code === "intent_not_found" || error.code === "session_changed";
}

export type SummaryTerm = "amount" | "fee" | "net" | "recipient" | "network" | "scope" | "host";

export interface SummaryRow {
  term: SummaryTerm;
  value: string;
  /** A decimal USDT figure, formatted exactly; the rest is shown as the BFF wrote it. */
  money: boolean;
}

/** The terms a person confirms, in reading order. A term the hub does not state is left out. */
export function summaryRows(summary: IntentSummary): SummaryRow[] {
  const rows: SummaryRow[] = [];
  const money = (term: SummaryTerm, value: string | null) => {
    if (value !== null) rows.push({ term, value, money: true });
  };
  money("amount", summary.amount);
  money("fee", summary.fee);
  money("net", summary.net);
  rows.push({ term: "recipient", value: summary.recipient, money: false });
  if (summary.network !== null) rows.push({ term: "network", value: summary.network, money: false });
  rows.push({ term: "scope", value: summary.scope, money: false });
  rows.push({ term: "host", value: summary.hostOrigin, money: false });
  return rows;
}

export type CapTerm = "perOp" | "per24h" | "outstanding" | "bookFill24h";

/** A mandate's caps in reading order; the book-fill cap only when the service asked for one. */
export function mandateCapRows(caps: MandateCaps): { term: CapTerm; value: string }[] {
  const rows: { term: CapTerm; value: string }[] = [
    { term: "perOp", value: caps.capPerOp },
    { term: "per24h", value: caps.cap24h },
    { term: "outstanding", value: caps.capOutstanding },
  ];
  if (caps.capBookFill24h !== "") rows.push({ term: "bookFill24h", value: caps.capBookFill24h });
  return rows;
}
