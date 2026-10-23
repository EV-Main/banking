import { createSentrySink } from "@evinvest/error-monitoring";

export { isChunkLoadError } from "./chunk-load-error.ts";

/**
 * Reports a lazily loaded module that never arrived and was degraded around rather than
 * thrown. Nothing else would see it: the screen stays up, so no error boundary fires.
 *
 * Sentry is imported lazily, as `features/kyc/api/kyc-client.ts` does: a static import
 * would put the SDK in the static graph of every screen that lazy-loads anything, to
 * report something that happens on no normal run. Unconfigured, it is a no-op.
 */
export function reportChunkError(error: unknown, where: string): void {
  const reported = error instanceof Error ? error : new Error(String(error));
  void import("@sentry/react").then(
    (Sentry) => createSentrySink(Sentry).reportError(reported, { where }),
    () => {},
  );
}
