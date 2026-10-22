// The account lives in concierge, not in this cabinet: its page is on the platform origin
// (`evinvest.ltd`), not under this app's base path. Callers pass the origin they read
// themselves, because the cabinet reads it through `config` and the chip bundle gets it
// inlined at build time (it has no `process`).

export type AccountSection = "profile" | "security" | "notifications";

/**
 * Concierge's account page. Without an origin it stays path-only, which is right on the
 * platform itself, where the cabinet is served from the same origin.
 */
export function platformAccountHref(origin: string | undefined, section: AccountSection): string {
  return `${origin?.replace(/\/+$/, "") ?? ""}/admin/account?section=${section}`;
}
