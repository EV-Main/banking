"use client";

// Browser → shell KYC client (site-root `/api/kyc/start`, `/api/kyc/status`). Verification is
// identity-plane work and the identity plane is shell-owned, exactly like `/api/auth/sessions`
// — hence `scope: "shell"`, no zone prefix. Transport, CSRF and the session pre-flight/replay
// belong to `@/shared/lib/api-client`; the wire shapes belong to ./kyc-contract; which refusal
// is which outcome belongs to ./start-outcome. What is left here is the I/O those three share.

import { createSentrySink } from "@evinvest/error-monitoring";

import { parseStartResponse, parseStatus, type KycStatus } from "@/features/kyc/api/kyc-contract";
import { classifyRefusal } from "@/features/kyc/api/start-outcome";
import { providerUrl } from "@/features/kyc/lib/provider-url";
import { extraProviderHosts } from "@/shared/config/kyc-provider";
import { RequestError, requestJson } from "@/shared/lib/api-client";

/**
 * `unavailable` is its own outcome rather than a failure because "no vendor will open a
 * session" is a DESIGNED state of that route: it degrades to one 503 with a support address,
 * and the screen owes the user a different sentence for it than for a fault. Modelling it as
 * a thrown error would have the UI reconstructing the distinction from a status code.
 *
 * `stale` and `throttled` are here for the same reason and no other: neither has wording of
 * its own in the transport's table that fits — a 24-hour cap is not "give it a moment", and a
 * stale CSRF token is not "you don't have access to this". Everything that carries a code the
 * transport already words stays inside `failed` — this layer names outcomes, it does not
 * write sentences.
 */
export type KycStart =
  | { kind: "started"; redirectUrl: string }
  | { kind: "unavailable"; contact: string | null }
  | { kind: "stale" }
  | { kind: "throttled" }
  | { kind: "emailUnverified" }
  | { kind: "failed"; error: unknown };

export async function startVerification(): Promise<KycStart> {
  try {
    // No body on purpose: absent `tier` means the entry tier, and that policy is the
    // identity plane's. Restating `{ tier: 1 }` here would be a second place to change it.
    const data = await requestJson<unknown>("/api/kyc/start", { method: "POST", scope: "shell" });
    const started = parseStartResponse(data);
    const url = providerUrl(started?.redirectUrl ?? null, extraProviderHosts(), selfHost());
    if (url !== null) return { kind: "started", redirectUrl: url };
    // A redirect we refused is not the same event as an answer we could not read, even
    // though the reader is owed the same sentence for both. Somebody tried to send an
    // authenticated cabinet user to a host that is not the vendor's — or the vendor moved
    // and nobody said so — and the reader's response to "please try again" is to try again,
    // not to write in. Unreported, that lasts exactly as long as nobody looks.
    if (started !== null) reportRefusedRedirect(started.redirectUrl);
    return { kind: "failed", error: unusable() };
  } catch (error) {
    return classify(error);
  }
}

/**
 * The caller's tier and their running case. `null` ONLY for a plane that does not have the
 * route: a cabinet deployed ahead of the concierge release that adds it (#75) gets a 404
 * here and must keep working exactly as it does today, falling back to the tier the profile
 * already carries.
 *
 * Everything else is re-raised. Flattening a 5xx, a network failure or an expired session
 * into the same `null` would make "the route is not deployed yet" and "the plane is down"
 * one state, which no screen could then render differently — and would swallow
 * `SessionExpiredError` before the keeper that moves to /login ever sees it.
 */
export async function fetchKycStatus(): Promise<KycStatus | null> {
  // A GET: no CSRF token, and nothing is spent by asking.
  const data = await requestJson<unknown>("/api/kyc/status", { scope: "shell" }).catch((error: unknown) => {
    if (error instanceof RequestError && error.status === 404) return null;
    throw error;
  });
  return data === null ? null : parseStatus(data);
}

/**
 * A 200 we cannot act on. Shaped as a `RequestError` with status 0 — the transport's own
 * idiom for "no HTTP verdict", as used for a network failure — so the row renders every
 * outcome through the one `errorMessage` boundary instead of carrying prose of its own.
 */
function unusable(): RequestError {
  return new RequestError("We couldn't start verification. Please try again.", 0, "err.kycStartFailed");
}

/** `undefined` during SSR of this client component, where there is no location to compare to. */
function selfHost(): string | undefined {
  return typeof window === "undefined" ? undefined : window.location.host;
}

/**
 * The host only. The rest of a session URL is the vendor's session id — a credential, and
 * `NEXT_PUBLIC_SENTRY_DSN` ships to every browser, so what goes out of here is the one field
 * that answers "whose host was it".
 *
 * Sentry is imported lazily, the way `@evinvest/error-monitoring`'s own provider imports it:
 * a static import here would put the SDK in the static graph of every screen that can start
 * verification, to report something that happens on no normal run. Unconfigured (no DSN,
 * local dev) the import resolves and `captureException` is a no-op.
 */
function reportRefusedRedirect(redirectUrl: string): void {
  let host: string;
  try {
    host = new URL(redirectUrl).host;
  } catch {
    host = "<unparseable>";
  }
  void import("@sentry/react").then(
    (Sentry) => createSentrySink(Sentry).reportError(new Error("kyc: refused the identity plane's redirect host"), { host }),
    () => {},
  );
}

function classify(error: unknown): KycStart {
  if (!(error instanceof RequestError)) {
    // Includes SessionExpiredError, whose own code already says to sign in again.
    return { kind: "failed", error };
  }
  const refusal = classifyRefusal(error.status, error.body);
  // `plain` is every refusal `shared/lib/api-client`'s own table already words — including
  // `internal`, which that table now keys to `err.serverUnavailable` rather than having this
  // layer restate the English sentence for it.
  return refusal.kind === "plain" ? { kind: "failed", error } : refusal;
}
