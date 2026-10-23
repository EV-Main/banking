/**
 * The vendor's URL is handed to us by our own identity plane, but it lands in
 * `window.location`, so it is checked here before anyone is sent to it.
 *
 * Two checks, in order of what they buy:
 *
 *  · **scheme** — a `javascript:` or `data:` URL in `window.location.assign` executes in THIS
 *    origin. One comparison keeps a confused provider from becoming an XSS in the cabinet.
 *  · **host** — the scheme check alone stops nothing a real attacker would do, because any
 *    `https:` URL passes it (#193). A swapped vendor domain or a misconfigured plane sends
 *    the user off the cabinet on a click they were told to trust, carrying a passport and a
 *    selfie with it.
 *
 * The allowlist is never empty, which is the whole difference from the first draft of this
 * file. That one took the host list from `NEXT_PUBLIC_KYC_PROVIDER_HOST` alone and degraded
 * to the scheme check when it was unset — and a `NEXT_PUBLIC_*` value is inlined at BUILD
 * time, so an image built without it (flake.nix sets no such variable) carried the control in
 * name only, in every environment including production. So {@link VENDOR_SESSION_HOST} is a
 * constant here, exactly as it is on the plane, and the variable only ADDS to it.
 *
 * The plane's own check is not a reason to soften this one: `redirect_is_trustworthy` lives
 * on concierge's `fen/kyc-start-contract` (#76), which is neither merged nor deployed, so
 * until it ships this function is the only host check anywhere in the flow.
 */

/**
 * Where Didit serves session pages — NOT the API host in `DIDIT_BASE_URL`
 * (`verification.didit.me`), which is a different name and never appears in a redirect.
 *
 * A constant and not an env var for the same reason concierge keeps it as one: it is a fact
 * about the vendor, like the `/v3/session/` path beside it there, and a knob is one more
 * value to carry through the deploy chain for a decision nobody is in a position to make at
 * 3am. The drift this accepts is real and bounded: if the vendor moves this domain, the plane
 * and the cabinet are two edits rather than one, and between them every start dead-ends at
 * `err.kycStartFailed` — noisily, and with the refused host in Sentry, rather than silently.
 * `NEXT_PUBLIC_KYC_PROVIDER_HOST` is the escape hatch that does not need a release.
 */
export const VENDOR_SESSION_HOST = "verify.didit.me";

/**
 * @param extraHosts - Deployment-specific hosts, already split and lower-cased
 *   (`shared/config/kyc-provider`).
 * @param selfHost - The cabinet's own `location.host`, admitted because `KYC_STUB` on the
 *   plane hands back a page on this very origin (`StubKyc::session_origins`). A same-origin
 *   redirect is not a hand-off to a third party, so there is nothing here to protect against.
 */
export function providerUrl(raw: string | null, extraHosts: readonly string[], selfHost?: string): string | null {
  if (!raw) return null;
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return null;
  }
  if (url.protocol !== "https:") return null;
  const allowed = [VENDOR_SESSION_HOST, ...extraHosts, ...(selfHost ? [selfHost.toLowerCase()] : [])];
  return allowed.includes(url.host.toLowerCase()) ? raw : null;
}
