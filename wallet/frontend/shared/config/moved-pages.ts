import { isLocale, type Locale } from "@evinvest/i18n";

import { BASE_PATH, cabinetPath } from "./base-path.ts";
import { type AccountSection, platformAccountHref } from "./platform.ts";

// The investor pages this cabinet no longer has (banking#584) are still linked from outside:
// the SA panel's "Manage account" points at `/cabinet/settings`, fee-policy mails already sent
// point at `/cabinet/invest/<service>`, and readers have bookmarks. Without a redirect those
// paths fall into the MFE catch-all and its `notFound()`.
//
// Only fixed origins and fixed paths go out. The two inputs that pass through, an
// investment slug and a settings section, are checked against a closed shape first, so
// no request can steer the reader to an address of its choosing.

export interface MovedPageOrigins {
  /** Concierge's origin; unset keeps the account link path-only (see `platformAccountHref`). */
  platform: string | undefined;
  /** The invest cabinet, its zone prefix included (`https://invest.evinvest.ltd/cabinet`). */
  investCabinet: string;
}

const SECTIONS: readonly AccountSection[] = ["profile", "security", "notifications"];
// The invest cabinet's service ids: the shape the deleted `invest/[service]` route served.
const SLUG = /^[A-Za-z0-9_-]{1,64}$/;


/** The investor page path after the zone prefix, when `pathname` is a cabinet page path. */
function zoneTail(pathname: string): { locale: Locale | null; tail: string[] } | null {
  const zone = BASE_PATH.slice(1);
  const [, first, ...rest] = pathname.split("/");
  if (first === zone) return { locale: null, tail: rest };
  if (isLocale(first) && rest[0] === zone) return { locale: first, tail: rest.slice(1) };
  return null;
}

function investTarget(investCabinet: string, tail: string[]): string {
  const root = `${investCabinet.replace(/\/+$/, "")}/invest`;
  const [service, sub, ...more] = tail;
  if (service === undefined) return root;
  if (!SLUG.test(service) || more.length > 0 || (sub !== undefined && sub !== "trade")) return root;
  return sub === undefined ? `${root}/${service}` : `${root}/${service}/trade`;
}

/**
 * Where a deleted investor page lives now, or `null` when `pathname` is not one of them.
 * A same-site target keeps the request's locale segment; without one it stays unprefixed,
 * and the proxy's locale repair picks the locale on the next request, as for any old link.
 */
export function movedPageTarget(pathname: string, search: string, origins: MovedPageOrigins): string | null {
  const at = zoneTail(pathname);
  if (!at) return null;
  const tail = at.tail.filter(Boolean);
  const sameSite = (path: `/${string}`) => (at.locale ? cabinetPath(at.locale, path) : `${BASE_PATH}${path}`);

  if (tail[0] === "invest") return investTarget(origins.investCabinet, tail.slice(1));
  if (tail.length === 1) {
    switch (tail[0]) {
      case "settings": {
        const asked = new URLSearchParams(search).get("section");
        // Security is where the panel's "Manage account" meant to land.
        return platformAccountHref(origins.platform, SECTIONS.find((known) => known === asked) ?? "security");
      }
      case "profile":
        return platformAccountHref(origins.platform, "profile");
      case "notifications":
        return platformAccountHref(origins.platform, "notifications");
      case "operations":
      case "dashboard":
        return sameSite("/wallet/activity");
    }
  }
  if (tail.length === 2 && tail[0] === "wallet" && tail[1] === "withdraw") return sameSite("/wallet/action/withdraw");
  return null;
}
