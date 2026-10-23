"use client";

// Ties this browser's PostHog person to the signed-in account, once per session.
//
// This is the cabinet's ONE deliberate exception to the hard rule that observability goes
// through `@evinvest/analytics` and never a vendor SDK by hand: the shared seam is
// `capture(event, props)` only, and `identify` has no place in it yet. The vendor call is
// confined to this module — nothing else in the cabinet imports `posthog-js` — until
// `@evinvest/analytics` grows `identify()`, at which point this file becomes a call to it.
//
// The ordering is the whole difficulty. The provider boots posthog-js lazily: it imports
// the SDK in an effect and runs `init` on the first capture that carries a key (its own
// mount `$pageview`). `identify` before `init` is a no-op with a console warning, and an
// effect below the provider can easily run first. So this waits for the SDK's own
// `__loaded` flag rather than assuming the provider has gone first, polling on a short
// tick for a bounded while — and gives up quietly, leaving the next call free to try again.

// Relative with the extension, like every module a node test imports: the runner resolves
// no `@/` alias.
import { config } from "../../config.ts";

/** The slice of the posthog-js client this module needs; a stub satisfies it in tests. */
export interface IdentifyClient {
  /** Set by `init`; false until the provider has booted the SDK. */
  __loaded: boolean;
  identify(distinctId: string): void;
  /** Drops the identified person so the next account on this tab starts anonymous. */
  reset(): void;
}

export interface IdentifierOptions {
  /** Whether analytics is configured at all — false means never load the SDK. */
  enabled: () => boolean;
  /** Brings in the SDK; the default is the dynamic import the provider also makes. */
  load: () => Promise<IdentifyClient>;
  /** Gap between `__loaded` checks. */
  pollMs?: number;
  /** How long to keep checking before giving up on this call. */
  timeoutMs?: number;
  /** Injectable clock and sleep, so the wait is testable without real time. */
  now?: () => number;
  wait?: (ms: number) => Promise<void>;
}

export interface Identifier {
  /**
   * Identify the person as `userId`. Resolves `true` when the SDK call was made, `false`
   * when it was already made for this id, analytics is off, or the SDK never came up in
   * time. A different id after a previous one resets the SDK first (an account switch on
   * the same tab must not merge two people).
   */
  identify: (userId: string) => Promise<boolean>;
  /** Forget the person — on sign-out, so the next visitor on this tab is nobody again. */
  reset: () => void;
  /** The id the SDK currently carries, or null. */
  current: () => string | null;
}

export function createIdentifier(options: IdentifierOptions): Identifier {
  const pollMs = options.pollMs ?? 50;
  const timeoutMs = options.timeoutMs ?? 10_000;
  const now = options.now ?? Date.now;
  const wait = options.wait ?? ((ms) => new Promise<void>((resolve) => setTimeout(resolve, ms)));

  let identified: string | null = null;
  let inflight: Promise<boolean> | null = null;
  let client: IdentifyClient | null = null;

  async function ready(): Promise<IdentifyClient | null> {
    client ??= await options.load();
    const deadline = now() + timeoutMs;
    while (!client.__loaded) {
      if (now() >= deadline) return null;
      await wait(pollMs);
    }
    return client;
  }

  async function run(userId: string): Promise<boolean> {
    const sdk = await ready();
    if (!sdk) return false;
    if (identified === userId) return false;
    if (identified !== null) sdk.reset();
    sdk.identify(userId);
    identified = userId;
    return true;
  }

  return {
    identify(userId) {
      if (!options.enabled() || identified === userId) return Promise.resolve(false);
      // One attempt at a time: a second caller while the SDK is still coming up joins the
      // first rather than racing it to `identify`.
      inflight ??= run(userId).finally(() => {
        inflight = null;
      });
      return inflight;
    },
    reset() {
      if (identified === null) return;
      identified = null;
      client?.reset();
    },
    current: () => identified,
  };
}

/** The cabinet's identifier, wired to the same `posthog-js` the provider boots. */
export const identity: Identifier = createIdentifier({
  enabled: () => typeof window !== "undefined" && Boolean(config.public.posthogKey),
  load: () => import("posthog-js").then((mod) => mod.default),
});
