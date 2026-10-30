// The account lives in concierge, not in this wallet host: its page is on the platform origin
// (`evinvest.ltd`), not under this app's base path. Callers pass the origin they read
// themselves, because the wallet host reads it through `config` and the KYC row reads it too.

export type AccountSection = "profile" | "security" | "notifications";

/**
 * Concierge's account page. Without an origin it stays path-only, which is right on the
 * platform itself, where the wallet host is served from the same origin.
 */
export function platformAccountHref(origin: string | undefined, section: AccountSection): string {
  return `${origin?.replace(/\/+$/, "") ?? ""}/account?section=${section}`;
}
