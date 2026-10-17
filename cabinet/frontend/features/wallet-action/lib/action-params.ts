// What a wallet action page takes from its URL — names and addresses, never money. The
// amount, the address and the payee are read from the server (an invoice banking issued,
// the reader's own wallet); the URL only says which invoice, which scope, where to report
// and where to go back to, and each of those is checked here before anything trusts it.
//
// Pure and import-free so `node --test` runs it as is.

export interface ScopeHost {
  origins: readonly string[];
  returnPaths: readonly string[];
}

export interface ActionParams {
  scope: string | null;
  invoice: string | null;
  /** The id of a mandate request banking stored; its terms are read from the server, never here. */
  request: string | null;
  /** The opener origin to post the result to — a registered origin of `scope`, or null. */
  opener: string | null;
  /** The opener's nonce, echoed in the result; null when absent or malformed. */
  state: string | null;
  /** Where a navigated (not popped-up) action sends the reader back to, or null. */
  returnTo: string | null;
}

const ID = /^[A-Za-z0-9_-]{1,128}$/;
// `@evinvest/wallet` draws 128 bits and writes them as 32 lowercase hex digits.
const STATE = /^[0-9a-f]{32}$/;

function id(raw: string | null): string | null {
  return raw !== null && ID.test(raw) ? raw : null;
}

function parse(raw: string | null): URL | null {
  if (raw === null || raw.length > 2048 || /[\u0000-\u001F\u007F\\]/.test(raw)) return null;
  try {
    return new URL(raw);
  } catch {
    return null;
  }
}

/**
 * The opener counts only when it is exactly one of the scope's origins — scheme, host and
 * port, nothing after. Without a scope there is no one registered to answer.
 */
function openerOf(raw: string | null, host: ScopeHost | undefined): string | null {
  const url = parse(raw);
  if (!url || !host) return null;
  // A path, query, hash or credentials all make `href` longer than the bare origin.
  if (url.href !== `${url.origin}/`) return null;
  return host.origins.includes(url.origin) ? url.origin : null;
}

/**
 * Back to this page's own origin (the full wallet navigating in place), or to a scope origin
 * plus one of its registered paths byte for byte. Never with a query or hash: those can carry
 * tokens that have no business on the way back from a money action.
 */
function returnToOf(raw: string | null, pageOrigin: string, host: ScopeHost | undefined): string | null {
  const url = parse(raw);
  if (!url || url.search !== "" || url.hash !== "" || url.username !== "" || url.password !== "") return null;
  if (url.origin === pageOrigin) return url.origin + url.pathname;
  if (host && host.origins.includes(url.origin) && host.returnPaths.includes(url.pathname)) return url.origin + url.pathname;
  return null;
}

export function readActionParams(search: string, pageOrigin: string, hosts: Readonly<Record<string, ScopeHost>>): ActionParams {
  const query = new URLSearchParams(search);
  const scope = id(query.get("scope"));
  const host = scope !== null && Object.hasOwn(hosts, scope) ? hosts[scope] : undefined;
  const state = query.get("state");
  return {
    scope,
    invoice: id(query.get("invoice")),
    request: id(query.get("request")),
    opener: openerOf(query.get("opener"), host),
    state: state !== null && STATE.test(state) ? state : null,
    returnTo: returnToOf(query.get("return_to"), pageOrigin, host),
  };
}

export type ActionOutcome = "done" | "failed" | "cancelled";

/** `type` of the result message (`@evinvest/wallet` `WALLET_RESULT_MESSAGE`). */
export const WALLET_RESULT_MESSAGE = "ev:wallet-result";

/** The message an opener accepts, or null when there is no one to send it to. */
export function resultMessage(params: ActionParams, outcome: ActionOutcome, reference: string | null): { type: string; state: string; outcome: ActionOutcome; reference?: string } | null {
  if (params.opener === null || params.state === null) return null;
  const usable = reference !== null && reference !== "" && reference.length <= 128;
  return { type: WALLET_RESULT_MESSAGE, state: params.state, outcome, ...(usable ? { reference } : {}) };
}
