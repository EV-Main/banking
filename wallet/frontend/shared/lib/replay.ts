// The replay loop `./api-client.ts` wraps around a read. It lives apart from the
// transport, with no imports at all, because that is the only way it gets a test: the
// Node runner strips types but resolves no `@/` alias (see `./resource.ts`), so nothing
// that imports the transport can be loaded by `npm run test`.

/**
 * Answers a GET is replayed against before its failure is reported: the zone proxy's
 * bodyless 502/503/504 and a dropped connection.
 *
 * A pod rollout leaves windows of a second or two — the old BFF gone from the Service
 * before the proxy notices, a SIGTERM landing on an in-flight answer — in which a read
 * fails for no reason the reader can act on, and the banner it produced was being
 * answered with a manual reload. A read is safe to replay; a mutation is not (a POST
 * that half-executed must not run twice), so only `GET` goes through this. Two replays,
 * under two seconds in total: a genuine outage still surfaces, just not on the first blip.
 */
export const REPLAY_DELAYS_MS: readonly number[] = [400, 1200];
export const REPLAYABLE_STATUSES: ReadonlySet<number> = new Set([502, 503, 504]);

/**
 * Runs `attempt` once when `replayable` is off, otherwise up to `delays.length + 1`
 * times, pausing `delays[i]` before replay `i`. A transient status or a thrown attempt
 * triggers a replay; the last attempt's answer — or its error — is passed on as is.
 */
export async function replaying(
  attempt: () => Promise<Response>,
  replayable: boolean,
  delays: readonly number[] = REPLAY_DELAYS_MS,
): Promise<Response> {
  for (let index = 0; ; index += 1) {
    const last = !replayable || index === delays.length;
    try {
      const res = await attempt();
      if (last || !REPLAYABLE_STATUSES.has(res.status)) return res;
    } catch (error) {
      if (last) throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, delays[index]));
  }
}
