// The single point where this app reads its environment. Every other module
// imports from `config` instead of touching `process.env`.
//
// Backed by @evinvest/settings — the TypeScript mirror of the `settings` Cargo
// feature of ev_lib. Settings are created lazily on first access so tests that
// mutate `process.env` between calls still work; `assertConfig()` (called from
// `instrumentation.ts` at server boot) forces validation before the server
// accepts traffic. The client/server split is automatic: only NEXT_PUBLIC_*
// keys reach the browser bundle; accessing a server key there is a no-op.

import { createSettings, bool, optional as opt, requiredIn, str, url, withDefault } from "@evinvest/settings";

// ── Lazy settings — re-created on every access ─────────────────────────
// No caching: tests mutate `process.env` between calls, and the overhead of
// validating 8 vars is negligible. `assertConfig()` forces a validation at
// server boot (CrashLoopBackOff on failure) before any traffic is accepted,
// matching the `@evinvest/settings` "fail at boot" contract.

function getSettings() {
  return createSettings({
    server: {
      // Where next.config.ts rewrites same-origin /api/* to (the Rust BFF).
      // Required — always set by the Nix build env and container contract.
      CABINET_BACKEND_URL: url(),
      // `__Host-`/Secure cookies need HTTPS; explicit override, else infer from NODE_ENV.
      AUTH_COOKIE_SECURE: opt(bool()),
      // Extra CSP/MFE allow-list origins not present in mfe-registry.json.
      MFE_ALLOWED_ORIGINS: opt(str()),
      // The concierge auth web surface, reached server-to-server (the same value the
      // conductor rewrites /api/auth/* to). Lets the proxy renew a lapsing access cookie
      // before a server render reads it. Unset ⇒ no server-side renewal: the browser's
      // SessionKeeper still rotates the cookie, the first server read just may find it stale.
      AUTH_WEB_URL: opt(url()),
      // Next.js sets NODE_ENV during build and at runtime; unset in tests.
      NODE_ENV: opt(str()),
    },
    clientPrefix: "NEXT_PUBLIC_",
    client: {
      NEXT_PUBLIC_MFE_ALLOWED_ORIGINS: opt(str()),
      // Host(s) the identity plane is allowed to hand back as a verification redirect,
      // comma-separated. Optional: the plane refuses a foreign host itself, and an unset
      // value here degrades to the https-only check rather than blocking verification.
      NEXT_PUBLIC_KYC_PROVIDER_HOST: opt(str()),
      // Unset ⇒ product analytics is a silent no-op: the provider never loads posthog-js.
      // (An empty string counts as unset — `@evinvest/settings` default — which is how the
      // flake spells "no key yet".) The key is public (it ships in the bundle), so it is a
      // build-time literal in flake.nix, not a pod secret — see `cabinetApp.env` there.
      NEXT_PUBLIC_POSTHOG_KEY: opt(str()),
      NEXT_PUBLIC_POSTHOG_HOST: opt(str()),
      // Unset ⇒ browser error monitoring is a silent no-op: the wallet host's
      // client-side crashes reach nobody. Acceptable locally, not in a deploy.
      NEXT_PUBLIC_SENTRY_DSN: requiredIn(opt(str()), "production"),
      // The Service-Arb panel Home links to. Unset ⇒ no card: a link to nowhere is worse
      // than no link. Public; a build-time literal in flake.nix like the PostHog key.
      NEXT_PUBLIC_SA_PANEL_URL: opt(url()),
      // The platform origin (concierge's account page lives there). Unset ⇒ account links
      // stay path-only, which only resolves on the platform's own origin. Public; a
      // build-time literal in flake.nix.
      NEXT_PUBLIC_PLATFORM_URL: opt(url()),
      // The invest cabinet, zone prefix included: the investor pages moved there, and the
      // proxy sends their old addresses in this wallet host to it. Public; a build-time literal in
      // flake.nix, with the prod value as the default so no build loses the redirect.
      NEXT_PUBLIC_INVEST_CABINET_URL: withDefault(url(), "https://invest.evinvest.ltd/cabinet"),
      // Cloudflare Turnstile site key for the sign-in dialog; locally Cloudflare's
      // always-pass test key `1x00000000000000000000AA`. Unset, the dialog's credential
      // forms stay disabled — the identity plane refuses them without a token anyway.
      NEXT_PUBLIC_TURNSTILE_SITE_KEY: requiredIn(opt(str()), "production"),
    },
    // `requiredIn` matches against this. NODE_ENV is what Next.js already sets
    // and what every other check here reads, so it stays the single name for
    // "which environment is this" rather than a second, drifting APP_ENV.
    //
    // `next build` is the exception, and must not resolve to `production`: the build
    // runs with NODE_ENV=production, but a NEXT_PUBLIC_* value is inlined from the
    // BUILD environment while the deploy only supplies the DSN to the running pod — so
    // a production-required client key can never be satisfied here, and instead fails
    // page-data collection (the whole wallet host image stops building). Same build-phase
    // carve-out `assertConfig()` already makes; the boot assert is what enforces this.
    profile: process.env.NEXT_PHASE === "phase-production-build" ? "build" : process.env.NODE_ENV,
    runtimeEnv: {
      CABINET_BACKEND_URL: process.env.CABINET_BACKEND_URL,
      AUTH_COOKIE_SECURE: process.env.AUTH_COOKIE_SECURE,
      MFE_ALLOWED_ORIGINS: process.env.MFE_ALLOWED_ORIGINS,
      AUTH_WEB_URL: process.env.AUTH_WEB_URL,
      NODE_ENV: process.env.NODE_ENV,
      NEXT_PUBLIC_MFE_ALLOWED_ORIGINS: process.env.NEXT_PUBLIC_MFE_ALLOWED_ORIGINS,
      NEXT_PUBLIC_KYC_PROVIDER_HOST: process.env.NEXT_PUBLIC_KYC_PROVIDER_HOST,
      NEXT_PUBLIC_POSTHOG_KEY: process.env.NEXT_PUBLIC_POSTHOG_KEY,
      NEXT_PUBLIC_POSTHOG_HOST: process.env.NEXT_PUBLIC_POSTHOG_HOST,
      NEXT_PUBLIC_SENTRY_DSN: process.env.NEXT_PUBLIC_SENTRY_DSN,
      NEXT_PUBLIC_SA_PANEL_URL: process.env.NEXT_PUBLIC_SA_PANEL_URL,
      NEXT_PUBLIC_PLATFORM_URL: process.env.NEXT_PUBLIC_PLATFORM_URL,
      NEXT_PUBLIC_INVEST_CABINET_URL: process.env.NEXT_PUBLIC_INVEST_CABINET_URL,
      NEXT_PUBLIC_TURNSTILE_SITE_KEY: process.env.NEXT_PUBLIC_TURNSTILE_SITE_KEY,
    },
  });
}

// ── Derived values (mirror the old config getters) ─────────────────────

function computeAuthCookieSecure(): boolean {
  const s = getSettings();
  return s.AUTH_COOKIE_SECURE ?? s.NODE_ENV === "production";
}

function computeIsProduction(): boolean {
  return getSettings().NODE_ENV === "production";
}

function computeIsDevelopment(): boolean {
  return getSettings().NODE_ENV === "development";
}

// ── Backward-compatible `config` export ────────────────────────────────
// The rest of the codebase imports `{ config }` from this module. Keep the
// shape exactly — lazy getters backed by @evinvest/settings validators.
//
// On the client, `getSettings()` returns only client keys; server getters
// short-circuit to undefined when `window` exists (matches the old behavior
// where `process.env[name]` returns undefined in the browser).

const _isServer = typeof window === "undefined";

export const config = ((): Readonly<{
  backendUrl: string;
  authCookieSecure: boolean;
  mfeAllowedOrigins: string | undefined;
  authWebUrl: string | undefined;
  isProduction: boolean;
  isDevelopment: boolean;
  public: Readonly<{
    mfeAllowedOrigins: string | undefined;
    kycProviderHost: string | undefined;
    posthogKey: string | undefined;
    posthogHost: string | undefined;
    sentryDsn: string | undefined;
    saPanelUrl: string | undefined;
    platformUrl: string | undefined;
    investCabinetUrl: string;
    turnstileSiteKey: string | undefined;
  }>;
}> => {
  return Object.freeze({
    get backendUrl(): string {
      return _isServer ? getSettings().CABINET_BACKEND_URL : (undefined as unknown as string);
    },
    get authCookieSecure(): boolean {
      return _isServer ? computeAuthCookieSecure() : false;
    },
    get mfeAllowedOrigins(): string | undefined {
      return _isServer ? getSettings().MFE_ALLOWED_ORIGINS : undefined;
    },
    get authWebUrl(): string | undefined {
      return _isServer ? getSettings().AUTH_WEB_URL : undefined;
    },
    get isProduction(): boolean {
      return _isServer ? computeIsProduction() : false;
    },
    get isDevelopment(): boolean {
      return _isServer ? computeIsDevelopment() : true;
    },
    public: Object.freeze({
      get mfeAllowedOrigins(): string | undefined {
        return getSettings().NEXT_PUBLIC_MFE_ALLOWED_ORIGINS;
      },
      get kycProviderHost(): string | undefined {
        return getSettings().NEXT_PUBLIC_KYC_PROVIDER_HOST;
      },
      get posthogKey(): string | undefined {
        return getSettings().NEXT_PUBLIC_POSTHOG_KEY;
      },
      get posthogHost(): string | undefined {
        return getSettings().NEXT_PUBLIC_POSTHOG_HOST;
      },
      get sentryDsn(): string | undefined {
        return getSettings().NEXT_PUBLIC_SENTRY_DSN;
      },
      get saPanelUrl(): string | undefined {
        return getSettings().NEXT_PUBLIC_SA_PANEL_URL;
      },
      get platformUrl(): string | undefined {
        return getSettings().NEXT_PUBLIC_PLATFORM_URL;
      },
      get investCabinetUrl(): string {
        return getSettings().NEXT_PUBLIC_INVEST_CABINET_URL;
      },
      get turnstileSiteKey(): string | undefined {
        return getSettings().NEXT_PUBLIC_TURNSTILE_SITE_KEY;
      },
    }),
  });
})();

/**
 * Eagerly assert the config surface is valid. Called from `instrumentation.ts`
 * at server boot — fails the deploy (CrashLoopBackOff) before any traffic is
 * accepted. With lazy settings, this is the moment validation actually runs.
 *
 * During `next build` the instrumentation gate (`NEXT_RUNTIME === "nodejs"`)
 * prevents this from evaluating in the Edge bundle.
 */
export function assertConfig(): void {
  if (process.env.NEXT_PHASE === "phase-production-build") return;
  // Touching the getters triggers `getSettings()` → creates + validates.
  void config.backendUrl;
  void config.authCookieSecure;
  void config.mfeAllowedOrigins;
  void config.authWebUrl;
  void config.isProduction;
  void config.isDevelopment;
  void config.public.mfeAllowedOrigins;
  void config.public.kycProviderHost;
  void config.public.posthogKey;
  void config.public.posthogHost;
  void config.public.sentryDsn;
  void config.public.saPanelUrl;
  void config.public.platformUrl;
  void config.public.investCabinetUrl;
  void config.public.turnstileSiteKey;
}

// Keep the old helpers for any remaining callers outside this module.
export function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`missing required env var ${name}`);
  return value;
}

export function optional(name: string): string | undefined {
  return process.env[name] || undefined;
}
