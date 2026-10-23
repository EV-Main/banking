// A code-split module whose "download" the test lands or fails by hand. A stub module
// (see `module-hooks.mjs`, `stubs`) awaits `downloadFor(name, case)` at its top level, so
// `import()` of it settles only when the test says; the test holds the other end through
// `fakeDownload(name, case)`. Keyed by the importer's `?case=`, as `fake-chart-engine.ts` is.

const KEY = Symbol.for("cabinet.test.downloads");

interface Pending {
  resolve: () => void;
  reject: (error: unknown) => void;
}

interface Channel {
  downloads: number;
  pending: Pending[];
  waiting: ((download: Pending) => void)[];
}

function channel(name: string, forCase: string): Channel {
  const registry = ((globalThis as Record<symbol, Map<string, Channel> | undefined>)[KEY] ??= new Map());
  const key = `${name}#${forCase}`;
  let found = registry.get(key);
  if (!found) {
    found = { downloads: 0, pending: [], waiting: [] };
    registry.set(key, found);
  }
  return found;
}

/** Called by a stub module at its top level: parks the import until the test decides. */
export function downloadFor(name: string, forCase: string): Promise<void> {
  const ch = channel(name, forCase);
  ch.downloads += 1;
  return new Promise<void>((resolve, reject) => {
    const download = { resolve, reject };
    const waiter = ch.waiting.shift();
    if (waiter) waiter(download);
    else ch.pending.push(download);
  });
}

export interface FakeDownload {
  /** Imports of the stub that reached it so far. */
  readonly downloads: number;
  /** Lets the oldest pending import land, waiting for it to start if need be. */
  arrive(): Promise<void>;
  /** Fails the oldest pending import, waiting for it to start if need be. */
  fail(error: unknown): Promise<void>;
}

export function fakeDownload(name: string, forCase: string): FakeDownload {
  const ch = channel(name, forCase);
  const next = (): Promise<Pending> => {
    const download = ch.pending.shift();
    return download ? Promise.resolve(download) : new Promise((resolve) => ch.waiting.push(resolve));
  };
  return {
    get downloads() {
      return ch.downloads;
    },
    arrive: async () => (await next()).resolve(),
    fail: async (error) => (await next()).reject(error),
  };
}

/** What a bundler rejects a chunk that never arrived with (Turbopack and webpack alike). */
export function chunkLoadError(message = "Failed to load chunk /cabinet/_next/static/chunks/test.js"): Error {
  const error = new Error(message);
  error.name = "ChunkLoadError";
  return error;
}
