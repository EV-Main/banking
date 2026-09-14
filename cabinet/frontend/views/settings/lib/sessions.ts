// Session-list display helpers: what device a captured User-Agent describes, and the meta
// line under it.

import { Laptop, type LucideIcon, Smartphone } from "lucide-react";

import type { Translate } from "@evinvest/i18n";

import type { Session } from "@/shared/contracts";

// Both helpers take the translator rather than returning finished English: they are called
// from exactly one client component (`sessions-section.tsx`), which already holds one, and
// keeping them plain functions means this module stays free of React.

// Best-effort device label from the captured User-Agent — enough to recognise a device,
// not a parser. Mobile UAs get the phone glyph.
export function deviceOf(ua: string | undefined, t: Translate): { label: string; icon: LucideIcon } {
  const u = (ua ?? "").toLowerCase();
  if (!u) return { label: t("settings.device.unknown", "Unknown device"), icon: Laptop };
  const mobile = /iphone|android|mobile/.test(u);
  // Browser and OS names are proper nouns and stay as they are; only the two generic
  // fallbacks are words rather than names.
  const browser = /edg/.test(u) ? "Edge" : /firefox|fxios/.test(u) ? "Firefox" : /chrome|crios/.test(u) ? "Chrome" : /safari/.test(u) ? "Safari" : t("settings.device.browser", "Browser");
  const os = /iphone|ipad|ios|crios|fxios/.test(u) ? "iOS" : /android/.test(u) ? "Android" : /mac os|macintosh/.test(u) ? "macOS" : /windows/.test(u) ? "Windows" : /linux/.test(u) ? "Linux" : t("settings.device.generic", "device");
  return { label: t("settings.device.label", "{browser} · {os}", { browser, os }), icon: mobile ? Smartphone : Laptop };
}

export function metaOf(s: Session, name: string, t: Translate): string {
  const ip = (s.ip ?? "").trim();
  const when = s.current ? t("settings.session.activeNow", "Active now") : lastSeen(s.last_seen, t);
  return [name, ip || null, when].filter(Boolean).join(" · ");
}

function lastSeen(value: number | string | undefined, t: Translate): string {
  const secs = Number(value ?? 0);
  if (!Number.isFinite(secs) || secs <= 0) return t("settings.session.activeRecently", "active recently");
  const diff = Date.now() - secs * 1000;
  if (diff < 60_000) return t("settings.session.activeNowLower", "active now");
  const mins = Math.floor(diff / 60_000);
  if (mins < 60) return t("settings.session.lastActiveMinutes", "last active {n}m ago", { n: mins });
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return t("settings.session.lastActiveHours", "last active {n}h ago", { n: hrs });
  return t("settings.session.lastActiveDays", "last active {n}d ago", { n: Math.floor(hrs / 24) });
}
