// Pure rules of the grant page, kept apart so they run under `node --test` (relative
// imports only: `@/` does not resolve there).

export interface RequestStamps {
  expires_at: string;
}

/** A request that can still be granted. An unreadable stamp counts as over: nothing is granted on a guess. */
export function isRequestAlive(r: RequestStamps, nowSeconds: number): boolean {
  const expires = Number(r.expires_at);
  return Number.isFinite(expires) && expires > nowSeconds;
}

export interface CapSource {
  cap_per_op: string;
  cap_24h: string;
  cap_outstanding: string;
  cap_book_fill_24h: string;
}

export type CapKind = "perOp" | "per24h" | "outstanding" | "bookFill24h";

/**
 * The caps to show, in order, each exactly as the server wrote it. The book-fill cap is
 * listed only when the server sent one; the other three are always part of a request.
 */
export function capRows(r: CapSource): { kind: CapKind; value: string }[] {
  const rows: { kind: CapKind; value: string }[] = [
    { kind: "perOp", value: r.cap_per_op },
    { kind: "per24h", value: r.cap_24h },
    { kind: "outstanding", value: r.cap_outstanding },
  ];
  if (r.cap_book_fill_24h !== "") rows.push({ kind: "bookFill24h", value: r.cap_book_fill_24h });
  return rows;
}

/** "0" is the server's "until revoked"; anything else is a unix-seconds end. */
export function endsWhenRevoked(stamp: string): boolean {
  return stamp === "0";
}

export interface HeldMandate {
  scope: string;
  client_id: string;
  revoked_at: string;
  replaced_at: string;
  expires_at: string;
}

/**
 * The live mandate a grant of `request` would replace: same service in the same scope. The
 * server decides what is replaced; this only tells the reader before they confirm.
 */
export function replacedBy<T extends HeldMandate>(request: { scope: string; client_id: string }, held: readonly T[], nowSeconds: number): T | null {
  return (
    held.find((m) => {
      if (m.scope !== request.scope || m.client_id !== request.client_id) return false;
      if (m.revoked_at !== "0" || m.replaced_at !== "0") return false;
      const end = Number(m.expires_at);
      return !(end > 0 && end <= nowSeconds);
    }) ?? null
  );
}

/** The server's answer for a request that cannot be shown: 404 for any unreadable id. */
export function isUnavailable(error: unknown): boolean {
  return typeof error === "object" && error !== null && Reflect.get(error, "status") === 404;
}

const KNOWN_OPERATIONS: readonly string[] = ["reserve", "charge", "book_fill"];

/**
 * Whether a request is safe to put in front of the reader. Fails closed: an operation this
 * page does not know (the BFF writes an unrecognised enum as "unspecified"), no operation
 * at all, a book-fill grant without its cap, or a request for another scope than the one the
 * URL (and so the result's host) named — each would show terms the reader cannot judge.
 */
export function isShowable(r: { scope: string; operations: readonly string[]; cap_book_fill_24h: string }, urlScope: string | null): boolean {
  if (urlScope !== null && urlScope !== r.scope) return false;
  if (r.operations.length === 0 || !r.operations.every((op) => KNOWN_OPERATIONS.includes(op))) return false;
  return !r.operations.includes("book_fill") || r.cap_book_fill_24h !== "";
}

/** A grant's answer that is already revoked or replaced is not a permission the reader now holds. */
export function grantStillHolds(m: { revoked_at: string; replaced_at: string }): boolean {
  return m.revoked_at === "0" && m.replaced_at === "0";
}
