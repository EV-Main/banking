// Security response headers for the cabinet host document.
//
// The cabinet renders authenticated, money-moving surfaces and runs third-party
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

/**
 * The `ws://` or `wss://` form of the origin this document was served from, for
 * `connect-src` — or null when the host is unknown or unusable.
 *
 * This exists because `'self'` does not reliably cover a websocket. CSP Level 3 made
 * `'self'` match a same-origin `wss:` handshake and current browsers implement that, but
 * the behaviour was absent for years and the failure mode is the worst kind: the socket is
 * blocked before it opens, with no error the page can catch — it simply never connects, and
 * the consilium quietly falls back to polling forever on the browsers that got it wrong.
 * Naming the origin explicitly costs one token and removes the question.
 *
 * The host comes from the request rather than an env var because the cabinet is a zone
 * mounted under the conductor's domain: the public origin is whatever the conductor was
 * asked for, and only the request knows it. `x-forwarded-*` win over `host` for the same
 * reason — behind the conductor, `host` is the internal upstream name, and a CSP naming
 * that origin would allow a socket nobody opens while blocking the one the browser does.
 *
 * A host is emitted only if it parses as one. The header is attacker-controllable, and a
 * malformed or injected value must fall out of the allow-list rather than into it.
 */
export function websocketOrigin(host: string | null | undefined, forwardedProto?: string | null): string | null {
  if (!host) return null;
  // The first entry of a comma-joined forwarded chain is the client-facing one.
  const proto = (forwardedProto ?? "").split(",")[0]?.trim().toLowerCase();
  const scheme = proto === "http" ? "ws" : proto === "https" ? "wss" : IN_PRODUCTION ? "wss" : "ws";
  try {
    const url = new URL(`${scheme}://${host.split(",")[0]!.trim()}`);
    // `new URL` tolerates a path, a userinfo section and a query; an origin must have none
    // of them, and CSP would silently take the whole string as a source expression.
    return url.host && url.pathname === "/" && !url.username && !url.search ? url.origin : null;
  } catch {
    return null;
  }
}

/** Cloudflare Turnstile's script and frame, which the credential forms carry. */
const TURNSTILE_ORIGIN = "https://challenges.cloudflare.com";

/**
 * The Content-Security-Policy for the host document, bound to a per-request nonce.
 *
 * `socketOrigin` is the value of {@link websocketOrigin} for this request. It is optional
 * so that a caller with no request in hand (the tests, any static evaluation) still gets a
 * complete policy — one that is stricter, never looser.
 */
export function contentSecurityPolicy(nonce: string, socketOrigin?: string | null): string {
  const script = ["'self'", `'nonce-${nonce}'`, TURNSTILE_ORIGIN, ...mfeOrigins()];
  if (IN_DEVELOPMENT) script.push("'unsafe-eval'");
  // The consilium's revision stream is same-origin (`/cabinet/api/owners/consilium/ws`,
  // rewritten to the BFF), so this adds a scheme, not a new host to trust.
  const connect = ["'self'", ...connectOrigins(), ...(socketOrigin ? [socketOrigin] : [])];
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
    // cabinet page) compresses its recording in a worker it builds from a `blob:` URL —
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
