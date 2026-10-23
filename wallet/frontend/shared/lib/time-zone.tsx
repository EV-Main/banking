"use client";

import { createContext, useContext, useSyncExternalStore, type ReactNode } from "react";

import { isTimeZone, TIME_ZONE_COOKIE } from "./time-zone-name.ts";

// Which zone a component formats its times in.
//
// Always the device's — except while hydrating HTML the server rendered, when it is the
// zone the server rendered in (the reader's cookie, else UTC). Hydration has to reproduce
// the server's text exactly, and the server cannot know the device's zone on a first visit
// or after the device moved. `useSyncExternalStore` is what makes the hand-over safe: its
// server snapshot is used for the server render AND for hydration, then the client
// snapshot takes over in a normal re-render — no mismatch, and in the usual case (cookie
// matches the device) nothing changes on screen at all.

const RenderedTimeZone = createContext<string | undefined>(undefined);

/** Set once, in the signed-in layout, from the zone the server formats in. */
export function TimeZoneProvider({ value, children }: { value: string; children: ReactNode }) {
  return <RenderedTimeZone value={value}>{children}</RenderedTimeZone>;
}

function deviceTimeZone(): string | undefined {
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  return isTimeZone(zone) ? zone : undefined;
}

const NO_SUBSCRIPTION = () => () => undefined;

/**
 * The zone to pass to a formatter. `undefined` means the runtime's own zone, which is what
 * every formatter did before there was a server render to agree with.
 */
export function useTimeZone(): string | undefined {
  const rendered = useContext(RenderedTimeZone);
  return useSyncExternalStore(NO_SUBSCRIPTION, deviceTimeZone, () => rendered);
}

/** Remember the device's zone for the next server render. A no-op when it is already stored. */
export function rememberTimeZone(): void {
  const zone = deviceTimeZone();
  if (!zone) return;
  // Same naming rule as the locale cookie (`./locale-cookie.ts`): `__Host-` needs Secure,
  // Secure needs https, so the protocol decides which name the browser will keep.
  const secure = location.protocol === "https:";
  const name = (secure ? "__Host-" : "") + TIME_ZONE_COOKIE;
  const value = encodeURIComponent(zone);
  if (document.cookie.split(";").some((part) => part.trim() === `${name}=${value}`)) return;
  document.cookie = `${name}=${value};path=/;max-age=${60 * 60 * 24 * 365};samesite=lax${secure ? ";secure" : ""}`;
}
