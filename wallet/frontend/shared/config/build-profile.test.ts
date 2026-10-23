// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The `requiredIn(…, "production")` client keys must bite at RUNTIME but not during
// `next build` — a NEXT_PUBLIC_* value is inlined from the build environment, while the
// deploy only supplies it to the running pod. Getting this wrong does not degrade
// gracefully: it failed page-data collection and blocked three releases (v0.2.25–27).
import assert from "node:assert/strict";
import test from "node:test";

import { config } from "../../config.ts";

// `CABINET_BACKEND_URL` is set by the `test` script, so an unset DSN is the only problem
// the validator can report — any throw below is unambiguously about the DSN.
function withEnv(vars: Record<string, string | undefined>, body: () => void) {
  const prev = Object.fromEntries(Object.keys(vars).map((k) => [k, process.env[k]]));
  for (const [k, v] of Object.entries(vars)) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  try {
    body();
  } finally {
    for (const [k, v] of Object.entries(prev)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  }
}

test("next build does not demand the production-only client keys", () => {
  withEnv({ NODE_ENV: "production", NEXT_PHASE: "phase-production-build", NEXT_PUBLIC_SENTRY_DSN: undefined }, () => {
    // Reading the getter is what the build does when a module touches `config` at
    // module scope (shared/config/security.ts) — it must not throw.
    assert.doesNotThrow(() => void config.public.sentryDsn);
    assert.equal(config.public.sentryDsn, undefined);
  });
});

test("a production runtime still demands the Sentry DSN", () => {
  withEnv({ NODE_ENV: "production", NEXT_PHASE: undefined, NEXT_PUBLIC_SENTRY_DSN: undefined }, () => {
    assert.throws(() => void config.public.sentryDsn, /NEXT_PUBLIC_SENTRY_DSN/);
  });
});

test("a set DSN is read back in both phases", () => {
  const dsn = "https://public@o0.ingest.sentry.io/0";
  for (const phase of ["phase-production-build", undefined]) {
    withEnv({ NODE_ENV: "production", NEXT_PHASE: phase, NEXT_PUBLIC_SENTRY_DSN: dsn, NEXT_PUBLIC_TURNSTILE_SITE_KEY: "site-key" }, () => {
      assert.equal(config.public.sentryDsn, dsn);
    });
  }
});

test("a production runtime demands the Turnstile site key", () => {
  withEnv({ NODE_ENV: "production", NEXT_PHASE: undefined, NEXT_PUBLIC_SENTRY_DSN: "https://public@o0.ingest.sentry.io/0", NEXT_PUBLIC_TURNSTILE_SITE_KEY: undefined }, () => {
    assert.throws(() => void config.public.turnstileSiteKey, /NEXT_PUBLIC_TURNSTILE_SITE_KEY/);
  });
});

test("development is unaffected — the DSN stays optional", () => {
  withEnv({ NODE_ENV: "development", NEXT_PHASE: undefined, NEXT_PUBLIC_SENTRY_DSN: undefined }, () => {
    assert.equal(config.public.sentryDsn, undefined);
  });
});
