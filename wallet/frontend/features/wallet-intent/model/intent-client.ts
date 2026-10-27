// The page's side of the wallet intent routes (docs/WALLET-INTENTS.md): the CSRF token, the
// create, the confirm. Every call goes to the wallet origin with its cookies; the BFF checks
// the page's Origin and Sec-Fetch-Site itself, so nothing here names the host.
//
// Transport and clock come in as arguments, and imports stay relative, so `node --test`
// runs it against a fake fetch.

import {
  confirmAgain,
  intentErrorOf,
  parseConfirmed,
  parseCsrf,
  parseIntent,
  type ConfirmedIntent,
  type CsrfToken,
  type Intent,
  type IntentKind,
  type IntentError,
  type IntentRequest,
  type IntentResult,
} from "../lib/intent.ts";

export interface IntentClientDeps {
  /** The wallet API base on the wallet origin, e.g. `https://evinvest.ltd/cabinet/api/wallet`. */
  apiBase: string;
  fetch: typeof fetch;
  /** Rotates the access cookie after a 401. False only when the shell says nobody is signed in. */
  renewSession: () => Promise<boolean>;
  now?: () => number;
}

export interface IntentClient {
  create(request: IntentRequest): Promise<IntentResult<Intent>>;
  /** Sends the confirm again while the act's outcome is unclear; the intent id keeps it one act. */
  confirm<K extends IntentKind>(intentId: string, proof?: string): Promise<IntentResult<ConfirmedIntent<K>>>;
  /**
   * The kinds this host may run in the page, from the same CSRF answer the posts use. Any
   * failure (routes off, signed out, offline) is no kinds: every action stays the popup.
   */
  kinds(): Promise<IntentKind[]>;
}

// The BFF accepts a token for 10 to 20 minutes. Reusing it for 5, by the local clock, stays
// inside that whatever the BFF's clock says.
const CSRF_REUSE_MS = 5 * 60_000;
export const CONFIRM_ATTEMPTS = 3;

type Answer = { status: number; body: unknown } | null;

const fail = (error: IntentError): { ok: false; error: IntentError } => ({ ok: false, error });

export function createIntentClient(deps: IntentClientDeps): IntentClient {
  const now = deps.now ?? Date.now;
  let csrf: { answer: CsrfToken; until: number } | null = null;
  // One request for every element and form on the page that asks at the same moment.
  let inFlight: Promise<IntentResult<CsrfToken>> | null = null;

  async function send(url: string, init: RequestInit): Promise<Answer> {
    try {
      const res = await deps.fetch(url, { ...init, credentials: "include", cache: "no-store" });
      const body: unknown = await res.json().catch(() => null);
      return { status: res.status, body };
    } catch {
      return null;
    }
  }

  function token(): Promise<IntentResult<CsrfToken>> {
    if (csrf !== null && csrf.until > now()) return Promise.resolve({ ok: true, value: csrf.answer });
    inFlight ??= fetchToken().finally(() => {
      inFlight = null;
    });
    return inFlight;
  }

  async function fetchToken(): Promise<IntentResult<CsrfToken>> {
    const get = () => send(`${deps.apiBase}/csrf`, { method: "GET", headers: { accept: "application/json" } });
    let answer = await get();
    if (answer?.status === 401) {
      if (!(await deps.renewSession())) return fail({ code: "signed_out" });
      answer = await get();
    }
    if (answer === null) return fail({ code: "network" });
    if (answer.status < 200 || answer.status >= 300) return fail(intentErrorOf(answer.status, answer.body));
    const parsed = parseCsrf(answer.body);
    if (parsed === null) return fail({ code: "malformed" });
    csrf = { answer: parsed, until: now() + CSRF_REUSE_MS };
    return { ok: true, value: parsed };
  }

  // A 401 and a stale CSRF token are both refused by the gate before anything runs, so each
  // earns one resend of the same body.
  async function post<T>(path: string, payload: object, parse: (body: unknown) => T | null): Promise<IntentResult<T>> {
    let renewed = false;
    let refreshed = false;
    for (;;) {
      const csrfToken = await token();
      if (!csrfToken.ok) return csrfToken;
      const answer = await send(`${deps.apiBase}${path}`, {
        method: "POST",
        headers: { accept: "application/json", "content-type": "application/json", "x-ev-csrf": csrfToken.value.token },
        body: JSON.stringify(payload),
      });
      if (answer === null) return fail({ code: "network" });
      if (answer.status === 401 && !renewed) {
        renewed = true;
        if (!(await deps.renewSession())) return fail({ code: "signed_out" });
        continue;
      }
      const error = answer.status >= 200 && answer.status < 300 ? null : intentErrorOf(answer.status, answer.body);
      if (error?.code === "csrf" && !refreshed) {
        refreshed = true;
        csrf = null;
        continue;
      }
      if (error !== null) return fail(error);
      const value = parse(answer.body);
      return value === null ? fail({ code: "malformed" }) : { ok: true, value };
    }
  }

  return {
    create: (request) => post("/intents", request, parseIntent),
    kinds: async () => {
      const answer = await token();
      return answer.ok ? answer.value.kinds : [];
    },
    async confirm<K extends IntentKind>(intentId: string, proof?: string) {
      const path = `/intents/${encodeURIComponent(intentId)}/confirm`;
      const payload = proof === undefined ? {} : { proof };
      let result = await post(path, payload, parseConfirmed<K>);
      for (let attempt = 1; attempt < CONFIRM_ATTEMPTS && !result.ok && confirmAgain(result.error); attempt++) {
        result = await post(path, payload, parseConfirmed<K>);
      }
      return result;
    },
  };
}
