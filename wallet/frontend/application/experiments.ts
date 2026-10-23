import type { ExperimentConfig } from "@evinvest/experiments";

/**
 * Central A/B experiment registry. Declare experiments here as
 * `key: { variants, weights }` (variant[0] is the control). The `proxy.ts`
 * boundary buckets each visitor into a sticky `ab_<key>` cookie across these,
 * Server Components read the assignment with `getVariant`, and client islands
 * report exposure/action events through `ExperimentTracker` wired to the
 * PostHog capture sink (`useCapture`).
 *
 * Empty until the first experiment lands — the surface is reserved so adding one
 * is a single entry, mirroring how the hub registers its (empty) gRPC services.
 */
export const experiments = {} as const satisfies ExperimentConfig;
