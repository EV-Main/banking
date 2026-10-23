// Server/runtime error monitoring (Sentry) for the BFF. `register` runs once on
// server boot and initialises the matching runtime (Node here — Next 16 proxy is
// Node-only) via @evinvest/error-monitoring; `onRequestError` reports errors
// thrown in server components and route handlers. Both no-op without SENTRY_DSN.
import {
  register as initMonitoring,
  captureRequestError,
} from "@evinvest/error-monitoring/next";

export async function register() {
  // Fail the boot, not the request: a required var missing throws here,
  // before the server accepts traffic. Gated on NEXT_RUNTIME (statically
  // inlined) so config.ts — and its process.exit — stays out of the Edge
  // bundle Next also compiles this file into.
  if (process.env.NEXT_RUNTIME === "nodejs") {
    const { orExit } = await import("@evinvest/settings");
    const { assertConfig } = await import("@/config");
    // Exit 78 (EX_CONFIG) instead of letting the throw surface as a generic
    // crash: "the config is wrong, restarting will not help", as opposed to the
    // dependency blip a restart does fix.
    orExit(() => assertConfig());
  }
  return initMonitoring();
}

export const onRequestError = captureRequestError;
