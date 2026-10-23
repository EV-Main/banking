/**
 * The EXTRA hosts a verification redirect may point at, beyond the vendor's own.
 *
 * Here rather than read from `@/config` inside the slice: every other reader of the env
 * funnel sits in `shared/` or at the root (`shared/config/security.ts`,
 * `shared/config/cookies.ts`, `shared/mfe/validate.ts`, `proxy.ts`), and the eslint
 * `no-restricted-properties` rule that centralises env access is only as strong as that
 * habit. `shared/config/support.ts` already holds a KYC-specific constant for the same
 * reason.
 *
 * Empty in every normal deploy, and that is the point: the host the vendor serves session
 * pages on is a fact about the vendor, not a deployment knob, so it is a constant in
 * `features/kyc/lib/provider-url` and the check can never ship inert. This variable exists
 * for the case the plane also admits — a sandbox, a staging tenant or a self-hosted base
 * URL, which concierge covers by admitting the origin of `DIDIT_BASE_URL` alongside its own
 * constant (`DiditKyc::session_origins`). An operator who points the plane at one of those
 * must set this too, or the cabinet refuses the redirect the plane just approved.
 */
import { config } from "@/config";

export function extraProviderHosts(): readonly string[] {
  return parseHosts(config.public.kycProviderHost);
}

/**
 * Comma-separated so a staging host can sit beside production in one variable. Hosts, not
 * origins: the scheme is fixed to https by the check itself, and a port is part of the host.
 */
function parseHosts(value: string | undefined): readonly string[] {
  if (!value) return [];
  return value
    .split(",")
    .map((host) => host.trim().toLowerCase())
    .filter((host) => host.length > 0);
}
