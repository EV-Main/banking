// The token-addressed approval page, as a rule rather than a list: it is
// `/owner-removal/<token>` — a path whose interesting part is different for every visitor,
// and which no equality test can name. It matches its own path exactly, or a path that
// continues with `/`: `/owner-removals` is a different route.

/**
 * The token-addressed approval page.
 *
 * It gets `Referrer-Policy: no-referrer` on top of the wallet host's default
 * `strict-origin-when-cross-origin`, which would still send the origin — and, to a
 * same-origin destination, the whole URL — onward. The URL *is* the credential here, so
 * every leak channel it can be pushed down (browser history is unavoidable; `Referer`,
 * proxy logs and third-party assets are not) is one to close.
 */
const TOKEN_APPROVAL_PATH = "/owner-removal";

/**
 * A real request path reduced to the zone-relative one the rules above are written in.
 *
 * `/{locale}/cabinet/owner-removal/x` → `/owner-removal/x`, and the zone root → `/`. A path that does
 * not carry the prefix is returned unchanged rather than mangled: the conductor owns other
 * routes on this origin, and the gate must not accidentally match one of them.
 */
export function zoneGatePath(pathname: string): string {
  const rest = pathname.replace(/^\/[a-z]{2}\/cabinet/, "");
  return rest === "" ? "/" : rest;
}

/** Whether a full request path is the token-addressed approval page. */
export function isTokenApprovalPath(pathname: string): boolean {
  const path = zoneGatePath(pathname);
  return path === TOKEN_APPROVAL_PATH || path.startsWith(`${TOKEN_APPROVAL_PATH}/`);
}

/**
 * Whether product analytics and error monitoring may observe this page.
 *
 * False for the token route, and the reason is that the URL *is* the credential there.
 * PostHog attaches `$current_url` to every event it sends, including the pageview it fires
 * on mount; Sentry stamps the same URL onto `request.url` and onto every fetch breadcrumb.
 * Mounting either on an approval page therefore ships a live, single-use approval token to
 * two third parties, off-origin, before the reader has done anything at all.
 *
 * `Referrer-Policy: no-referrer` does not help with this. That header governs the `Referer`
 * of outbound requests; it has no bearing on a URL an SDK reads out of `location` and puts
 * in a request body.
 *
 * The redaction this would otherwise call for has nowhere to live: neither
 * `@evinvest/analytics`'s `PostHogProvider` (`apiKey` / `host` / `capturePageview`) nor
 * `@evinvest/error-monitoring`'s `ErrorMonitoringProvider` (`dsn` / `environment` / sample
 * rates) exposes a `beforeSend` or sanitiser seam, and reaching past them to configure the
 * vendor SDKs by hand is exactly what AGENTS.md § Hard rules forbids. So the honest fix is
 * not to observe this page — which costs a pageview nobody is measuring and error reports
 * from one page, and removes the leak outright rather than filtering it.
 */
export function mayObserve(pathname: string): boolean {
  return !isTokenApprovalPath(pathname);
}
