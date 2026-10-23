// Security response headers for the wallet host host document.
//
// The wallet host renders authenticated, money-moving surfaces and runs third-party
// MFE code, so the host document ships a baseline of hardening headers. The CSP
// is the load-bearing one: MFE bundles are loaded as runtime ESM <script> by
// arbitrary registry-provided scriptUrls (shared/mfe/RemoteElement.tsx), so
// script-src cannot be a trivial 'self' — it must carry an explicit per-origin
// allow-list derived from the MFE registry (plus the observability endpoints the
// browser talks to), otherwise the CSP breaks legitimate remotes.
//
// Split of responsibilities: the request-invariant headers below are emitted
// statically from next.config.ts; the CSP is emitted per-request from proxy.ts
// because it carries a per-request nonce (so Next's own inline bootstrap scripts
// stay allowed without 'unsafe-inline').

import { readFileSync } from "node:fs";
import path from "node:path";

import { config } from "../../config.ts";

const IN_PRODUCTION = config.isProduction;
// `next dev` only (never build/start, never the test runner where NODE_ENV is
// unset): React + Turbopack use eval() in development for HMR and callstack
// reconstruction, so the strict no-eval CSP would break the dev server.
const IN_DEVELOPMENT = config.isDevelopment;

// Absolute-URL origins of MFE bundles, read once from the same registry the BFF
// serves. Relative scriptUrls (e.g. "/mfe/x.js") are same-origin and covered by
// 'self', so only cross-origin remotes contribute here. MFE_ALLOWED_ORIGINS adds
// any production CDN origins not present in the in-repo file (space/comma list).
function mfeOrigins(): string[] {
  const origins = new Set<string>();
  try {
    const file = path.join(process.cwd(), "mfe-registry.json");
    const entries = JSON.parse(readFileSync(file, "utf8")) as Array<{ scriptUrl?: string }>;
    for (const { scriptUrl } of entries) {
      const origin = absoluteOrigin(scriptUrl);
      if (origin) origins.add(origin);
    }
  } catch {
    // A missing/invalid registry must not crash header construction; same-origin
    // remotes still load under 'self', and a misconfigured CSP fails closed.
  }
  for (const o of splitList(config.mfeAllowedOrigins)) origins.add(o);
  return [...origins];
}

// Origins the browser opens network connections to (XHR/fetch/WebSocket), so
// connect-src lets them through. Same-origin /api/* is covered by 'self'; the
// observability endpoints are env-driven and contribute nothing when unset.
function connectOrigins(): string[] {
  const origins = new Set<string>(mfeOrigins());
  const posthog = absoluteOrigin(config.public.posthogHost);
  if (posthog) origins.add(posthog);
  const sentry = absoluteOrigin(config.public.sentryDsn);
  if (sentry) origins.add(sentry);
  return [...origins];
}

function absoluteOrigin(value: string | undefined): string | null {
  if (!value) return null;
  try {
    return new URL(value).origin;
  } catch {
    return null; // relative URL → same-origin, already covered by 'self'.
  }
}

function splitList(value: string | undefined): string[] {
  return (value ?? "").split(/[\s,]+/).filter(Boolean);
}

/** Cloudflare Turnstile's script and frame, which the credential forms carry. */
const TURNSTILE_ORIGIN = "https://challenges.cloudflare.com";

/**
 * The Content-Security-Policy for the host document, bound to a per-request nonce.
 */
export function contentSecurityPolicy(nonce: string): string {
  const script = ["'self'", `'nonce-${nonce}'`, TURNSTILE_ORIGIN, ...mfeOrigins()];
  if (IN_DEVELOPMENT) script.push("'unsafe-eval'");
  const connect = ["'self'", ...connectOrigins()];
  return [
    `default-src 'self'`,
    `base-uri 'self'`,
    `object-src 'none'`,
    `frame-ancestors 'none'`,
    `form-action 'self'`,
    `script-src ${script.join(" ")}`,
    // The sign-in dialog's challenge renders in Cloudflare's frame.
    `frame-src ${TURNSTILE_ORIGIN}`,
    // styled-components / Tailwind inject runtime <style>; 'unsafe-inline' here is
    // style-only and does not weaken the XSS-relevant script-src.
    `style-src 'self' 'unsafe-inline'`,
    `img-src 'self' data: blob:`,
    `font-src 'self'`,
    // Sentry Replay (mounted through `@evinvest/error-monitoring`'s provider on every
    // zone page) compresses its recording in a worker it builds from a `blob:` URL —
    // and it starts recording in buffer mode whenever the error sample rate is above
    // zero, DSN or not, so this is a production fact and not a dev-overlay one. Stated
    // here rather than loosened in `script-src`: without a `worker-src` the browser
    // falls back to `script-src`, and `blob:` there would admit scripts, not just workers.
    `worker-src 'self' blob:`,
    `connect-src ${connect.join(" ")}`,
    ...(IN_PRODUCTION ? [`upgrade-insecure-requests`] : []),
  ].join("; ");
}

/**
 * The per-request CSP nonce, read back from the header proxy.ts set, so Server
 * Components that render their own inline scripts (e.g. next-themes) can stamp it
 * and stay allowed under script-src. `null` outside a request (e.g. static
 * prerender), where there is no inline script to protect. `next/headers` is
 * imported lazily so next.config.ts can import this module at config-eval time.
 */
export async function requestNonce(): Promise<string | null> {
  const { headers } = await import("next/headers");
  const csp = (await headers()).get("content-security-policy");
  return csp?.match(/'nonce-([^']+)'/)?.[1] ?? null;
}

/**
 * Request-invariant security headers emitted statically by next.config.ts.
 * X-Frame-Options is the belt to the CSP's frame-ancestors braces (and covers
 * routes the proxy matcher skips); HSTS is production-only (it requires HTTPS).
 */
export function staticSecurityHeaders(): Array<{ key: string; value: string }> {
  const headers = [
    { key: "X-Frame-Options", value: "DENY" },
    { key: "X-Content-Type-Options", value: "nosniff" },
    { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
  ];
  if (IN_PRODUCTION) {
    headers.push({ key: "Strict-Transport-Security", value: "max-age=63072000; includeSubDomains; preload" });
  }
  return headers;
}
