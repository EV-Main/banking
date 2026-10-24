// The wallet element's transport. Not `shared/lib/api-client`: that one resolves every path
// against the page it runs on, and on a scope host (sa.evinvest.ltd, invest.evinvest.ltd)
// the page is someone else's app. Here every call goes to the wallet's own origin — the one
// the bundle was served from — with the evinvest.ltd cookies (`credentials: "include"`).
// Reads only. Money moves through the intent routes alone (`./wallet-intents.ts`), whose
// terms the BFF computes and the person confirms.

import { apiPath } from "@/shared/config/base-path";

import { parseSignedIn, parseWalletBalance, parseWalletHistory, parseWithdrawNetworks, type WalletBalance, type WalletHistory } from "../lib/parse";

export type RemoteResult<T> = { ok: true; value: T } | { ok: false; reason: "signed_out" | "unavailable" };

// What the element shows: a glance at the newest rows, not the full timeline.
export const HISTORY_LIMIT = 20;

async function getJson(url: string): Promise<{ status: number; body: unknown } | null> {
  try {
    const res = await fetch(url, { credentials: "include", headers: { accept: "application/json" }, cache: "no-store" });
    const body: unknown = await res.json().catch(() => null);
    return { status: res.status, body };
  } catch {
    return null;
  }
}

/**
 * Asks the shell's session endpoint, which rotates the access cookie when the session is
 * alive. False only for an explicit "not signed in"; an unreachable shell is not a sign-out.
 */
export async function renewSession(origin: string): Promise<boolean> {
  const session = await getJson(new URL("/api/auth/session", origin).href);
  return !(session && session.status === 200 && !parseSignedIn(session.body));
}

/**
 * The access cookie lapses on a short TTL and nothing on a scope host renews it, so a 401
 * renews the session and replays once. Only an explicit "not signed in" is `signed_out`.
 */
async function read<T>(origin: string, path: `/${string}`, parse: (body: unknown) => T): Promise<RemoteResult<T>> {
  const url = new URL(apiPath(path), origin).href;
  let res = await getJson(url);
  if (res?.status === 401) {
    if (!(await renewSession(origin))) return { ok: false, reason: "signed_out" };
    res = await getJson(url);
    if (res?.status === 401) return { ok: false, reason: "signed_out" };
  }
  if (!res || res.status < 200 || res.status >= 300) return { ok: false, reason: "unavailable" };
  return { ok: true, value: parse(res.body) };
}

export function fetchBalance(origin: string): Promise<RemoteResult<WalletBalance | null>> {
  return read(origin, "/api/wallet", parseWalletBalance);
}

export function fetchHistory(origin: string): Promise<RemoteResult<WalletHistory>> {
  return read(origin, `/api/operations?limit=${HISTORY_LIMIT}`, parseWalletHistory);
}

export function fetchWithdrawNetworks(origin: string): Promise<RemoteResult<string[]>> {
  return read(origin, "/api/wallet", parseWithdrawNetworks);
}
