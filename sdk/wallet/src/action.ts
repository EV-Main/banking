// Money actions on a scope host. The wallet there shows balances and history
// only; a withdrawal, transfer, payment or mandate happens on evinvest.ltd, in a
// popup (or, when the popup is blocked, a full navigation), and the result comes
// back to the opener by `postMessage`. Banking's mutating routes accept only
// `Origin: https://evinvest.ltd`, so nothing here can move money by itself.
//
// Importing is server-safe; `openWalletAction` touches `window` when called.

import { isSecureTransport } from '@evinvest/mfe';

import { WALLET_ORIGIN } from './elements';

/**
 * Cancelable, from the wallet elements: the user picked a money action.
 * `detail` is `{ href }`, one of {@link WALLET_ACTION_PATHS} on
 * `https://evinvest.ltd`. A host that takes it calls `preventDefault()` and
 * opens it; left alone, the element navigates.
 */
export const WALLET_ACTION_EVENT = 'ev:wallet-action';

/** `type` of the message the evinvest.ltd page posts to its opener. */
export const WALLET_RESULT_MESSAGE = 'ev:wallet-result';

/**
 * The money actions a host may open, as exact paths on {@link WALLET_ORIGIN}.
 * Anything else — a sibling page, a trailing slash, a deeper path — is not an
 * action, however it is spelled.
 */
export const WALLET_ACTION_PATHS = ['/wallet/pay', '/wallet/withdraw', '/wallet/move', '/wallet/mandate'] as const;

export type WalletActionPath = (typeof WALLET_ACTION_PATHS)[number];

/**
 * The only query parameters an action URL keeps: which invoice, which scope, which
 * mandate request. `request` names a server-side record of what a service asked for; the
 * mandate's terms are read from it, never from the URL.
 */
export const WALLET_ACTION_PARAMS = ['invoice', 'scope', 'request'] as const;

/** Query parameter telling the action page which origin to post its result to. */
export const OPENER_PARAM = 'opener';
/** Query parameter telling the action page where to send the user back to when navigated, not popped up. */
export const RETURN_TO_PARAM = 'return_to';
/** Query parameter carrying the nonce the action page echoes in its result message. */
export const STATE_PARAM = 'state';

const MAX_URL = 2048;
const MAX_REFERENCE = 128;
// An id with a separator or an encoded character in it can make the action
// page look up something other than what the element named; refused, not cleaned.
const ACTION_PARAM_VALUE = /^[A-Za-z0-9_-]{1,128}$/;
const DEFAULT_TIMEOUT_MS = 15 * 60_000;
const DEFAULT_POLL_MS = 500;
const POPUP_FEATURES = 'popup=yes,width=480,height=720';
/** One named window per page: a second action reuses it rather than stacking popups. */
const POPUP_NAME = 'ev-wallet-action';

function isActionPath(path: string): path is WalletActionPath {
  return WALLET_ACTION_PATHS.some(action => action === path);
}

/**
 * `raw` as a wallet action URL on `origin`, rebuilt from scratch, or null.
 * Only an exact {@link WALLET_ACTION_PATHS} path survives, with the first
 * `invoice`, `scope` and `request` values; every other parameter and the hash are
 * dropped. Refused outright: another origin, another path, credentials,
 * control characters and backslashes — browsers strip tabs and newlines and
 * read `\` as `/` before the check would see them — and an `invoice`,
 * `scope` or `request` that is present but not 1–128 of `A–Z a–z 0–9 _ -`.
 *
 * Contract of the action page on evinvest.ltd, which this URL opens:
 * - amount, address and recipient are never read from the URL — a payment's
 *   come from the invoice banking issued, server-side; the URL only names it;
 * - a mandate's service and caps likewise come from the mandate request banking
 *   stored, which `request` only names;
 * - `opener` is accepted only when its origin exactly matches one of the
 *   scope's registered origins, else ignored;
 * - `return_to` is accepted only when it is one of those origins plus a path
 *   byte for byte in the scope's `return_paths` — no query, no hash — else
 *   ignored;
 * - the result goes out as `opener.postMessage(msg, <opener origin>)`, never
 *   with `'*'`, and echoes `state` unchanged;
 * - the page is served with `frame-ancestors 'none'`, so no host can frame it
 *   and click through it.
 */
export function walletActionUrl(raw: unknown, origin: string = WALLET_ORIGIN): URL | null {
  if (typeof raw !== 'string' || raw.length === 0 || raw.length > MAX_URL) return null;
  if (/[\u0000-\u001F\u007F\\]/.test(raw)) return null;
  let parsed: URL;
  try {
    parsed = new URL(raw);
  } catch {
    return null;
  }
  if (parsed.origin !== origin || parsed.username !== '' || parsed.password !== '') return null;
  if (!isActionPath(parsed.pathname)) return null;
  const url = new URL(parsed.pathname, origin);
  for (const name of WALLET_ACTION_PARAMS) {
    const value = parsed.searchParams.get(name);
    if (value === null) continue;
    if (!ACTION_PARAM_VALUE.test(value)) return null;
    url.searchParams.set(name, value);
  }
  return url;
}

/** The action URL of a {@link WALLET_ACTION_EVENT}, or null when the detail is missing or not one. */
export function walletActionFromEvent(event: Event, origin: string = WALLET_ORIGIN): URL | null {
  const detail: unknown = event instanceof CustomEvent ? event.detail : undefined;
  if (typeof detail !== 'object' || detail === null) return null;
  return walletActionUrl(Reflect.get(detail, 'href'), origin);
}

/**
 * What the action page reports. `reference` names what was created (payout,
 * payment), when anything was.
 *
 * `done` is a hint for the UI — close a dialog, show "processing" — not proof
 * that money moved: the message comes from a window the host cannot inspect.
 * What is paid is decided by the money flow (`InvoicePaid`); a host that
 * unlocks anything on payment reads its status from its own backend.
 */
export interface WalletResult {
  outcome: 'done' | 'failed' | 'cancelled';
  reference: string | null;
}

const OUTCOMES: readonly WalletResult['outcome'][] = ['done', 'failed', 'cancelled'];

/**
 * A {@link WALLET_RESULT_MESSAGE} payload for the action opened with `state`,
 * or null for anything else on the channel — a message without the echoed
 * `state` belongs to another action, or to nobody. The outcome is what
 * matters — money may already have moved — so a reference that cannot be used
 * (empty, not a string, over 128 characters) reads as none rather than losing
 * the outcome. A long one is never cut: a truncated payment reference names a
 * different payment, or none.
 */
export function parseWalletMessage(data: unknown, state: string): WalletResult | null {
  if (typeof data !== 'object' || data === null) return null;
  if (Reflect.get(data, 'type') !== WALLET_RESULT_MESSAGE) return null;
  if (state === '' || Reflect.get(data, 'state') !== state) return null;
  const outcome: unknown = Reflect.get(data, 'outcome');
  const reference: unknown = Reflect.get(data, 'reference');
  const known = OUTCOMES.find(value => value === outcome);
  if (known === undefined) return null;
  const usable = typeof reference === 'string' && reference !== '' && reference.length <= MAX_REFERENCE;
  return { outcome: known, reference: usable ? reference : null };
}

/**
 * How a popup action ended. Every way is a value: the host decides what each
 * means. After `closed` or `timeout` the action may still have gone through —
 * the user can close the popup after paying — so the host re-reads the status
 * from its backend; it does not offer to repeat the action.
 */
export type WalletActionOutcome =
  | { status: 'result'; result: WalletResult }
  /** The user closed the popup before the page reported anything; the action's fate is unknown. */
  | { status: 'closed' }
  /** `window.open` was refused (popup blocker); navigate with {@link walletNavigationHref} instead. */
  | { status: 'blocked' }
  /** Another action's popup is still open on this page; it was focused instead. Report nothing for this click. */
  | { status: 'busy' }
  /** No result within `timeoutMs`; the popup was closed, the action's fate is unknown. */
  | { status: 'timeout' }
  /** `signal` aborted, before the popup opened or while it was open; the popup was closed. */
  | { status: 'aborted' }
  /** `url` is not a wallet action on `origin` ({@link walletActionUrl}); nothing was opened. */
  | { status: 'invalid_url' }
  /** No `crypto` to draw a state from; nothing was opened. */
  | { status: 'unsupported' };

export interface WalletActionOptions {
  /** The origin the result must come from. Default {@link WALLET_ORIGIN}; other values are for a staging stand. */
  origin?: string;
  /** Give up — and close the popup — after this long. Default 15 minutes. */
  timeoutMs?: number;
  /** How often to look whether the user closed the popup. Default 500 ms. */
  pollMs?: number;
  /** Aborting closes the popup and resolves `aborted`. */
  signal?: AbortSignal;
}

/**
 * `url` with this page's origin as {@link OPENER_PARAM} and a fresh
 * {@link STATE_PARAM}, or null without `crypto`. The state ties the
 * result to this one action: a page reloaded from history, or a second
 * evinvest.ltd tab, cannot answer for it.
 */
function withOpener(url: URL): { href: string; state: string } | null {
  let state: string;
  try {
    // `getRandomValues` works outside a secure context, where `randomUUID` is missing.
    state = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, '0')).join('');
  } catch {
    // `openWalletAction` promises never to reject.
    return null;
  }
  const next = new URL(url);
  next.searchParams.set(OPENER_PARAM, window.location.origin);
  next.searchParams.set(STATE_PARAM, state);
  return { href: next.href, state };
}

/**
 * The same action as a full navigation, for when the popup is blocked: the page
 * sends the user back to `returnTo` instead of posting. `returnTo` resolves
 * against this page and is cut to origin and path — the action page takes only
 * an exact path from the scope's `return_paths`, and query or hash can hold
 * tokens or ids that have no business in a URL on evinvest.ltd. One on another
 * origin, or over plain http to a real host, is replaced by the default: this
 * page's origin and path. Null when `url` is not a wallet action on `origin`
 * ({@link walletActionUrl}). Browser only.
 */
export function walletNavigationHref(url: URL, returnTo?: string, origin: string = WALLET_ORIGIN): string | null {
  const target = walletActionUrl(url.href, origin);
  if (!target) return null;
  target.searchParams.set(RETURN_TO_PARAM, sameOriginReturnTo(returnTo));
  return target.href;
}

function sameOriginReturnTo(returnTo: string | undefined): string {
  const fallback = window.location.origin + window.location.pathname;
  if (returnTo === undefined) return fallback;
  let back: URL;
  try {
    back = new URL(returnTo, window.location.href);
  } catch {
    return fallback;
  }
  // The navigation leaves this page for good: a foreign return address would
  // hand the user, fresh from a money action, to whoever named it.
  if (back.origin !== window.location.origin || !isSecureTransport(back)) return fallback;
  return back.origin + back.pathname;
}

// The popup of the action in flight on this page. Page-wide, like the window
// name: a second click anywhere — another button, the same one again — must
// not reload the first action's page under it, nor report a second result.
let inFlight: Window | null = null;

/**
 * Open a money action on evinvest.ltd in a popup and wait for its result. Only
 * a message from the popup itself, from `origin`, in the
 * {@link WALLET_RESULT_MESSAGE} shape and echoing this call's `state` counts;
 * everything else on the channel is ignored. While one action is in flight, another call focuses its popup and
 * resolves `busy`. A popup this call opened is closed whatever the outcome.
 * Call it inside the user's click — a popup opened later is blocked. Never
 * rejects. Browser only.
 */
export function openWalletAction(url: string | URL, options: WalletActionOptions = {}): Promise<WalletActionOutcome> {
  const origin = options.origin ?? WALLET_ORIGIN;
  const target = walletActionUrl(String(url), origin);
  if (!target) return Promise.resolve({ status: 'invalid_url' });
  if (options.signal?.aborted) return Promise.resolve({ status: 'aborted' });
  if (inFlight && !inFlight.closed) {
    inFlight.focus();
    return Promise.resolve({ status: 'busy' });
  }
  const opened = withOpener(target);
  if (!opened) return Promise.resolve({ status: 'unsupported' });
  const { href, state } = opened;
  // Not `noopener`: the page needs `window.opener` to post back. It is ours.
  const popup = window.open(href, POPUP_NAME, POPUP_FEATURES);
  if (!popup) return Promise.resolve({ status: 'blocked' });
  inFlight = popup;

  return new Promise<WalletActionOutcome>(resolve => {
    let settled = false;
    const settle = (outcome: WalletActionOutcome) => {
      if (settled) return;
      settled = true;
      if (inFlight === popup) inFlight = null;
      window.removeEventListener('message', onMessage);
      clearInterval(poll);
      clearTimeout(timer);
      options.signal?.removeEventListener('abort', onAbort);
      if (!popup.closed) popup.close();
      resolve(outcome);
    };
    const onMessage = (event: MessageEvent) => {
      // The source check stops another evinvest.ltd window from answering for this popup.
      if (event.origin !== origin || event.source !== popup) return;
      const result = parseWalletMessage(event.data, state);
      if (result) settle({ status: 'result', result });
    };
    const onAbort = () => settle({ status: 'aborted' });
    const poll = setInterval(() => {
      if (!popup.closed) return;
      clearInterval(poll);
      // A page that posts and then closes itself can be seen closed before its
      // message is dispatched here; one more macrotask lets the result win.
      setTimeout(() => settle({ status: 'closed' }), 0);
    }, options.pollMs ?? DEFAULT_POLL_MS);
    const timer = setTimeout(() => settle({ status: 'timeout' }), options.timeoutMs ?? DEFAULT_TIMEOUT_MS);
    window.addEventListener('message', onMessage);
    options.signal?.addEventListener('abort', onAbort, { once: true });
  });
}
