// The page's side of concierge's step-up routes: open a ceremony for one intent, close it
// with a passkey assertion or a mailed code, and get the proof back. The proof is returned to
// the caller and kept nowhere here: no field, no cache, no error text, no log.
//
// No CSRF header: concierge takes none on these routes (its `__Host-` cookie is unreadable
// from a host page), and its CORS allows `content-type` alone, so another header would fail
// the preflight. Transport comes in as an argument and imports stay relative, so `node --test`
// runs it against a fake fetch.

import type { StepupMethod } from "../lib/intent.ts";
import { parseCeremony, parseProof, stepupErrorOf, type StepupCeremony, type StepupError, type StepupResult } from "../lib/stepup.ts";

export interface StepupClientDeps {
  /** The step-up base on the wallet origin, e.g. `https://evinvest.ltd/api/auth/stepup`. */
  apiBase: string;
  fetch: typeof fetch;
  /** Rotates the access cookie after a 401. False only when the shell says nobody is signed in. */
  renewSession: () => Promise<boolean>;
}

export type CeremonyRequest = { method: "passkey" } | { method: "email_code"; summary: string; turnstileToken: string };

export type CeremonyAnswer = { response: Record<string, unknown> } | { code: string };

export interface StepupClient {
  begin(intentHash: string, request: CeremonyRequest): Promise<StepupResult<StepupCeremony>>;
  /** The proof for the ceremony's intent. Pass it straight to the confirm. */
  finish(ceremonyId: string, answer: CeremonyAnswer): Promise<StepupResult<string>>;
}

type Answer = { status: number; body: unknown } | null;

const fail = (error: StepupError): { ok: false; error: StepupError } => ({ ok: false, error });

export function createStepupClient(deps: StepupClientDeps): StepupClient {
  async function send(path: string, payload: object): Promise<Answer> {
    try {
      const res = await deps.fetch(`${deps.apiBase}${path}`, {
        method: "POST",
        credentials: "include",
        cache: "no-store",
        headers: { accept: "application/json", "content-type": "application/json" },
        body: JSON.stringify(payload),
      });
      const body: unknown = await res.json().catch(() => null);
      return { status: res.status, body };
    } catch {
      return null;
    }
  }

  // Concierge answers 401 for two things. `passkey_rejected` is the verdict on an assertion,
  // given after the ceremony was taken, so it is final and never resent. Any other 401 is the
  // session gate, refused before a ceremony opens or closes: one resend after renewing the
  // cookie spends nothing twice.
  async function post<T>(path: string, payload: object, parse: (body: unknown) => T | null): Promise<StepupResult<T>> {
    const sessionRefused = (a: Answer) => a?.status === 401 && stepupErrorOf(401, a.body).code !== "passkey_rejected";
    let answer = await send(path, payload);
    if (sessionRefused(answer)) {
      if (!(await deps.renewSession())) return fail({ code: "signed_out" });
      answer = await send(path, payload);
      if (sessionRefused(answer)) return fail({ code: "signed_out" });
    }
    if (answer === null) return fail({ code: "network" });
    if (answer.status < 200 || answer.status >= 300) return fail(stepupErrorOf(answer.status, answer.body));
    const value = parse(answer.body);
    return value === null ? fail({ code: "malformed" }) : { ok: true, value };
  }

  return {
    begin(intentHash, request) {
      const method: StepupMethod = request.method;
      const payload =
        request.method === "passkey"
          ? { intent_hash: intentHash, method }
          : { intent_hash: intentHash, method, summary: request.summary, turnstile_token: request.turnstileToken };
      return post("/options", payload, (body) => parseCeremony(body, method));
    },
    finish(ceremonyId, answer) {
      const payload = "code" in answer ? { ceremony_id: ceremonyId, code: answer.code } : { ceremony_id: ceremonyId, response: answer.response };
      return post("/verify", payload, parseProof);
    },
  };
}
