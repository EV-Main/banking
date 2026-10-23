// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { contentSecurityPolicy, staticSecurityHeaders, websocketOrigin } from "./security.ts";

test("CSP forbids inline/eval and clickjacking, and carries the nonce", () => {
  const csp = contentSecurityPolicy("testnonce123");
  const scriptSrc = csp.split("; ").find((d) => d.startsWith("script-src "));
  assert.ok(scriptSrc, "a script-src directive is present");
  assert.ok(scriptSrc.includes("'nonce-testnonce123'"), "script-src carries the per-request nonce");
  assert.ok(!scriptSrc.includes("'unsafe-inline'"), "script-src must not allow inline scripts");
  assert.ok(!scriptSrc.includes("'unsafe-eval'"), "script-src must not allow eval");
  assert.ok(csp.includes("frame-ancestors 'none'"), "frame-ancestors locks down framing");
  assert.ok(csp.includes("object-src 'none'"), "object-src is closed");
  assert.ok(csp.includes("base-uri 'self'"), "base-uri is locked to self");
});

test("CSP derives an explicit allow-list from cross-origin MFE/observability hosts", () => {
  const prev = { mfe: process.env.MFE_ALLOWED_ORIGINS, ph: process.env.NEXT_PUBLIC_POSTHOG_HOST };
  process.env.MFE_ALLOWED_ORIGINS = "https://cdn.example.com/remotes/x.js https://mfe.example.org";
  process.env.NEXT_PUBLIC_POSTHOG_HOST = "https://us.i.posthog.com";
  try {
    const csp = contentSecurityPolicy("n");
    const scriptSrc = csp.split("; ").find((d) => d.startsWith("script-src "))!;
    const connectSrc = csp.split("; ").find((d) => d.startsWith("connect-src "))!;
    assert.ok(scriptSrc.includes("https://cdn.example.com"), "registry origin is allow-listed in script-src");
    assert.ok(scriptSrc.includes("https://mfe.example.org"), "extra MFE origin is allow-listed");
    assert.ok(connectSrc.includes("https://us.i.posthog.com"), "observability origin is allow-listed in connect-src");
  } finally {
    if (prev.mfe === undefined) delete process.env.MFE_ALLOWED_ORIGINS;
    else process.env.MFE_ALLOWED_ORIGINS = prev.mfe;
    if (prev.ph === undefined) delete process.env.NEXT_PUBLIC_POSTHOG_HOST;
    else process.env.NEXT_PUBLIC_POSTHOG_HOST = prev.ph;
  }
});

test("the consilium socket origin reaches connect-src", () => {
  // Without this the browser blocks the handshake before it opens — silently, with nothing
  // the page can catch — and the owners' room degrades to polling for no visible reason.
  const csp = contentSecurityPolicy("n", websocketOrigin("cabinet.example.com", "https"));
  const connectSrc = csp.split("; ").find((d) => d.startsWith("connect-src "))!;
  assert.ok(connectSrc.includes("wss://cabinet.example.com"), "the socket origin is allow-listed");
  assert.ok(connectSrc.includes("'self'"), "same-origin XHR is still allowed");
});

test("the socket scheme follows the scheme the reader was served over", () => {
  assert.equal(websocketOrigin("cabinet.example.com", "https"), "wss://cabinet.example.com");
  assert.equal(websocketOrigin("localhost:3000", "http"), "ws://localhost:3000");
  // A forwarded chain names the client-facing hop first.
  assert.equal(websocketOrigin("cabinet.example.com", "https, http"), "wss://cabinet.example.com");
});

test("a host that is not a host is dropped, not allow-listed", () => {
  // `Host` is attacker-controllable. Anything that would smuggle a second source
  // expression, a path or credentials into the directive has to fall out of the list.
  for (const host of [null, undefined, "", "evil.com/../*", "user:pw@evil.com", "a b", "evil.com?x=1"]) {
    assert.equal(websocketOrigin(host, "https"), null, String(host));
  }
});

test("a CSP built without a request is complete and no looser", () => {
  // next.config.ts and the tests both evaluate this with no request in hand.
  const csp = contentSecurityPolicy("n");
  const connectSrc = csp.split("; ").find((d) => d.startsWith("connect-src "))!;
  assert.ok(!connectSrc.includes("ws://"), "no websocket scheme is allowed by default");
  assert.ok(!connectSrc.includes("wss://"), "no websocket scheme is allowed by default");
});

test("static headers cover frame, sniffing and referrer hardening", () => {
  const keys = staticSecurityHeaders().map((h) => h.key);
  assert.ok(keys.includes("X-Frame-Options"), "X-Frame-Options is present");
  assert.ok(keys.includes("X-Content-Type-Options"), "X-Content-Type-Options is present");
  assert.ok(keys.includes("Referrer-Policy"), "Referrer-Policy is present");
  const xfo = staticSecurityHeaders().find((h) => h.key === "X-Frame-Options")!;
  assert.equal(xfo.value, "DENY");
  const nosniff = staticSecurityHeaders().find((h) => h.key === "X-Content-Type-Options")!;
  assert.equal(nosniff.value, "nosniff");
});

test("workers may be built from blob: URLs, and only workers", () => {
  // Sentry Replay's compression worker is a `new Worker(URL.createObjectURL(blob))`.
  // Without its own directive the browser would judge it under script-src, where `blob:`
  // must never appear — so the allowance is scoped to worker-src and script-src stays shut.
  const csp = contentSecurityPolicy("testnonce123");
  const workerSrc = csp.split(";").map((s) => s.trim()).find((s) => s.startsWith("worker-src"));
  const scriptSrc = csp.split(";").map((s) => s.trim()).find((s) => s.startsWith("script-src"))!;
  assert.ok(workerSrc, "a worker-src directive is present");
  assert.ok(workerSrc.includes("blob:"), "worker-src admits blob: workers");
  assert.ok(workerSrc.includes("'self'"), "worker-src keeps same-origin workers");
  assert.ok(!scriptSrc.includes("blob:"), "script-src must not admit blob: scripts");
});
