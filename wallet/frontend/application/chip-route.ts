// The wallet host as a host of its own account chip: which `ev:navigate` events its router takes
// (shared/config/chip-element.ts). Only a localized zone page — `/{locale}/cabinet…` — is a
// route of this app. Everything else the chip links to stays a full load on purpose: the BFF
// and auth endpoints (`/api/auth/login` behind Switch account), other zones of the domain
// (multi-zone: a client transition cannot leave this app), and the unprefixed `/cabinet/…`,
// which only the proxy's locale repair knows how to serve.
//
// Relative imports, no `@/`: `node --test` runs this file and cannot resolve the alias.

import { isLocale } from "@evinvest/i18n";

import { isZonePage } from "../shared/config/base-path.ts";
import { CHIP_TAG } from "../shared/config/chip-element.ts";

/** Whether an event came out of an account chip element (its `composedPath()`). */
export function fromChip(path: readonly EventTarget[]): boolean {
  return path.some((node) => "localName" in node && node.localName === CHIP_TAG);
}

/** The path to `router.push` for an `ev:navigate` detail, or null to let the browser go. */
export function zoneRouteFor(detail: unknown): string | null {
  // Whatever dispatched the event wrote the detail, so it is read as outside input.
  if (typeof detail !== "object" || detail === null || !("href" in detail)) return null;
  const { href } = detail;
  // Path-absolute only: `//host/…` is protocol-relative, i.e. another origin.
  if (typeof href !== "string" || !href.startsWith("/") || href.startsWith("//")) return null;
  let url: URL;
  try {
    url = new URL(href, "https://cabinet.invalid");
  } catch {
    return null;
  }
  if (url.origin !== "https://cabinet.invalid") return null;
  const [, locale] = url.pathname.split("/");
  if (!isLocale(locale) || !isZonePage(url.pathname)) return null;
  return url.pathname + url.search + url.hash;
}
