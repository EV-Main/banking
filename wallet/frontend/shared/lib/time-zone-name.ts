// The reader's time zone as a cookie value — the one part of the zone handshake both the
// server and the browser need, so it is import-free and runs in either.
//
// Why a cookie at all: the cabinet shows times in the device's own zone, and a server
// render has no device. Without the zone, a server-rendered timeline groups and labels its
// rows in the server's zone (UTC in the container) and the browser hydrates them in the
// reader's — different text, a hydration error, and rows moving between day headings. The
// browser writes its zone here once (`application/layout/time-zone-sync.tsx`) and every
// later render formats in it (`shared/lib/time-zone.tsx`).

/** Cookie name without the `__Host-` prefix; the server adds it from config, the browser from the protocol. */
export const TIME_ZONE_COOKIE = "ev_tz";

/**
 * An IANA zone name `Intl` accepts. The value comes from a cookie anyone can write, so it is
 * checked by the formatter that will use it rather than trusted: an unknown zone would make
 * every `toLocaleString` on the page throw a RangeError.
 */
export function isTimeZone(value: string | undefined): value is string {
  if (!value || value.length > 64 || !/^[A-Za-z0-9_+\-/]+$/.test(value)) return false;
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: value });
    return true;
  } catch {
    return false;
  }
}
