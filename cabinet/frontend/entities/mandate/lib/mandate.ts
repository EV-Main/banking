// Pure rules of the mandate page, kept apart so they run under `node --test`.

export interface MandateStamps {
  scope: string;
  revoked_at: string;
  replaced_at: string;
  expires_at: string;
}

/** A mandate that still lets its service draw: not revoked, not replaced, not expired. */
export function isLive(m: MandateStamps, nowSeconds: number): boolean {
  if (m.revoked_at !== "0" || m.replaced_at !== "0") return false;
  const expires = Number(m.expires_at);
  return !(expires > 0 && expires <= nowSeconds);
}

/**
 * The mandates to show: live ones, narrowed to the scope the URL named when the server's
 * own list has any there. The URL only narrows what the server returned; it adds nothing.
 */
export function visibleMandates<T extends MandateStamps>(list: readonly T[], urlScope: string | null, nowSeconds: number): T[] {
  const live = list.filter((m) => isLive(m, nowSeconds));
  if (urlScope === null) return live;
  const narrowed = live.filter((m) => m.scope === urlScope);
  return narrowed.length > 0 ? narrowed : live;
}

/**
 * The BFF's grant gate (#524) answers 401 `step_up_required` when the session's last
 * interactive sign-in is older than five minutes; the page reads it as "sign in again", not
 * as a broken request. `requestJson` replays a 401 once after refreshing the session, which
 * cannot clear this verdict, so the error that arrives here is the replay's.
 */
export function isStepUpRequired(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  if (Reflect.get(error, "status") !== 401) return false;
  const body: unknown = Reflect.get(error, "body");
  return typeof body === "object" && body !== null && Reflect.get(body, "error") === "step_up_required";
}
