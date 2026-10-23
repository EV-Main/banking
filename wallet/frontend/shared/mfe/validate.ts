// The registry is the sole source of trust for which code runs in the cabinet
// origin (shared/mfe/RemoteElement.tsx injects each entry's bundle as first-party
// ESM), so it is validated against a strict schema before any entry is trusted:
// known shape, an SRI hash present for cross-origin bundles, and an origin on a
// fixed allow-list. Runs in both environments (the server loader and the browser
// RemoteElement) so the origin gate is enforced at injection time too.

import { config } from "../../config.ts";

import type { MfeEntry, MfeKind } from "./types";

const KINDS: readonly MfeKind[] = ["component", "page"];

// Extra cross-origin MFE/CDN origins not present as absolute URLs in the registry.
// Mirrors the CSP allow-list (shared/config/security.ts) — keep them in sync. The
// browser-readable NEXT_PUBLIC_ mirror lets RemoteElement re-check at injection
// time, where only inlined NEXT_PUBLIC_ env is available.
function extraAllowedOrigins(): string[] {
  const raw = config.mfeAllowedOrigins ?? config.public.mfeAllowedOrigins ?? "";
  return raw.split(/[\s,]+/).filter(Boolean);
}

/** A relative scriptUrl is same-origin (the cabinet itself), already trusted by 'self'. */
export function isSameOrigin(scriptUrl: string): boolean {
  try {
    new URL(scriptUrl);
    return false;
  } catch {
    return true;
  }
}

/**
 * Whether a bundle URL may be injected: same-origin relative URLs always pass;
 * cross-origin URLs must resolve to an origin on the allow-list. Anything that
 * does not parse to an http(s) origin is rejected.
 */
export function isAllowedScriptUrl(scriptUrl: string): boolean {
  if (typeof scriptUrl !== "string" || scriptUrl.length === 0) return false;
  if (isSameOrigin(scriptUrl)) return true;
  let origin: string;
  try {
    const url = new URL(scriptUrl);
    if (url.protocol !== "https:" && url.protocol !== "http:") return false;
    origin = url.origin;
  } catch {
    return false;
  }
  return new Set(extraAllowedOrigins()).has(origin);
}

function isEntry(value: unknown): value is MfeEntry {
  if (typeof value !== "object" || value === null) return false;
  const e = value as Record<string, unknown>;
  if (typeof e.name !== "string" || e.name.length === 0) return false;
  if (typeof e.tag !== "string" || e.tag.length === 0) return false;
  if (typeof e.scriptUrl !== "string" || !isAllowedScriptUrl(e.scriptUrl)) return false;
  if (typeof e.kind !== "string" || !KINDS.includes(e.kind as MfeKind)) return false;
  // Cross-origin bundles must carry an SRI hash, delivered with the URL.
  if (!isSameOrigin(e.scriptUrl) && (typeof e.integrity !== "string" || !e.integrity.startsWith("sha"))) return false;
  if (e.integrity !== undefined && typeof e.integrity !== "string") return false;
  return true;
}

/** Parse and validate a registry payload, rejecting unknown shapes/origins. Throws on failure. */
export function parseRegistry(value: unknown): MfeEntry[] {
  if (!Array.isArray(value)) throw new Error("mfe registry must be an array");
  return value.map((entry, i) => {
    if (!isEntry(entry)) throw new Error(`mfe registry entry ${i} failed validation (shape/origin/integrity)`);
    return entry;
  });
}
