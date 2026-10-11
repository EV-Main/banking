// Browser-side CSRF double-submit helper: read the readable `ev_csrf` cookie and
// build the header for mutating BFF requests. Standalone (no server imports) so it
// lives in `shared` — both the auth feature and other slices depend on it without
// crossing layers. The server counterpart is `features/auth/lib/csrf.ts` (verifyCsrf).

const CSRF_HEADER = "x-ev-csrf";

function readCookie(name: string): string | null {
  const prefix = `${name}=`;
  const found = document.cookie.split("; ").find((entry) => entry.startsWith(prefix));
  if (!found) return null;
  try {
    return decodeURIComponent(found.slice(prefix.length));
  } catch {
    // A malformed `%` sequence (a cookie another app on the parent domain wrote) would throw
    // out of every mutating request; no token is the failure the server already answers.
    return null;
  }
}

export function csrfHeader(): Record<string, string> {
  // Prefer the `__Host-` (host-locked) cookie so a subdomain-injected plain
  // `ev_csrf` can't shadow it; fall back to the dev (unprefixed) name.
  const token = readCookie("__Host-ev_csrf") ?? readCookie("ev_csrf");
  return token ? { [CSRF_HEADER]: token } : {};
}
