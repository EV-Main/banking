// Kept apart from `chunk-error.ts` and free of imports: that module pulls in the error
// monitor, and the pure view decisions that branch on this predicate are loaded by the
// Node test runner, which neither resolves `@/` nor wants a Sentry sink.

/**
 * A code-split chunk that failed to download. Both bundlers name the error this way, and
 * Turbopack caches the rejected chunk for the life of the page: asking again without a
 * reload replays the same failure, so the only honest recovery offered is a reload.
 */
export function isChunkLoadError(error: unknown): boolean {
  return error instanceof Error && error.name === "ChunkLoadError";
}
