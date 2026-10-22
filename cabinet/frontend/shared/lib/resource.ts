"use client";

// The cabinet's read cache: one value per endpoint, shared by every screen that reads it.
//
// Why this exists. Every view owned its own `useEffect(() => fetchX().then(setX))`, so the
// balance was re-fetched on Home, again on Wallet, again on Deposit, again on Withdraw,
// again on Invest — and each arrival was preceded by a skeleton. Nothing was wrong with any
// one of those reads; the problem was that leaving a page threw its answer away, so moving
// between two screens of the SAME account looked like loading a different account.
//
// Why not Next's fetch cache. `fetch(url, { next: { revalidate, tags } })` is a SERVER
// extension — it only applies to fetches Next itself issues while rendering. This cabinet
// reads nothing on the server: `/api/*` is a rewrite straight to the Rust BFF
// (next.config.ts), there are no route handlers, and every read is a browser call from a
// client component carrying the user's session cookie. So the cache has to live in the
// browser, at the one transport chokepoint that does exist (`shared/lib/api-client.ts`,
// wrapped by the entity clients). The vocabulary here is deliberately Next's — `revalidate`
// in seconds, `tags`, and a `revalidateTag()` that mutations call — because the semantics
// are the same and there is no reason to invent a second dialect for them.
//
// A server read does not replace this cache, it feeds it: a server component reads once for the first HTML and passes the answer down as a seed
// (`useSeededResource`), and from then on the value lives, ages and is invalidated here.
//
// Why not a store library. The repo already answers this: `shared/lib/use-session.ts` and
// `shared/lib/use-platform.ts` are hand-rolled copies of this same pattern, each with
// slightly different staleness rules. This is that pattern
// written once, with a policy per resource instead of per copy — and no new dependency
// (see AGENTS.md § Hard rules).
//
// The behaviour that removes the skeleton: `useResource` serves whatever is already in the
// cache on the FIRST render, synchronously. A revalidation, when the value has aged past
// its `revalidate` window, runs behind the figure already on screen and swaps it in place —
// so `isLoading` is true only when there is genuinely nothing to show yet, which is the
// only state that has ever earned a skeleton. A failed revalidation keeps the stale value
// and reports the error beside it: a money surface must not blank a balance because one
// poll timed out.

import { useCallback, useMemo, useSyncExternalStore } from "react";

/** Seconds a value stays fresh when a resource doesn't name its own window. */
const DEFAULT_REVALIDATE_S = 15;

/** sessionStorage namespace for `persist` resources — see `dropPersisted` for the scope rules. */
const PERSIST_PREFIX = "ev.cabinet.resource:";

export interface ResourceSnapshot<T> {
  /** The last known value, from this page or any earlier one. */
  data: T | undefined;
  /** The most recent attempt failed. Stale `data`, if any, is still present beside it. */
  error: Error | null;
  /** Nothing to show and nothing has failed — the only state that earns a skeleton. */
  isLoading: boolean;
  /** A refresh is running behind a value that is already on screen. */
  isValidating: boolean;
  /** Force a read now, ignoring freshness (a retry button, a pull-to-refresh). */
  refresh: () => Promise<void>;
}

export interface ResourceConfig<T, A extends unknown[]> {
  /** Stable identity — the cache key prefix, and what `clearResources` reports on. */
  name: string;
  /** The underlying read. One per endpoint; the entity clients supply these. */
  fetch: (...args: A) => Promise<T>;
  /** Key material for a parameterised read (`fundNav(service)`). Omit for a read with no arguments. */
  key?: (...args: A) => string;
  /** Seconds the cached value is served without a background refresh. */
  revalidate?: number;
  /** Invalidation groups a mutation can name through `revalidateTag`. */
  tags?: readonly string[];
  /**
   * Mirror into sessionStorage so a hard reload also paints from cache.
   *
   * NON-PERSONAL data only. The catalog of open funds and their NAVs qualify; a balance,
   * a position, a profile or an operation does not — those stay in memory, which dies with
   * the page. (`entities/user/model/identity-cache.ts` makes the same call for the
   * three identity fields it needs.)
   */
  persist?: boolean;
  /**
   * Whether these arguments describe a read worth making. A resource keyed on a value the
   * screen hasn't chosen yet (no fund selected) reports "not loading, no data" instead of
   * issuing a request the BFF would reject.
   */
  enabled?: (...args: A) => boolean;
  /**
   * Poll a mounted screen while `while(data)` holds — for a verdict that can land on the
   * backend with nobody bringing the tab back into focus to notice it (see
   * `entities/user/model/profile-resource.ts`). None of the clock's other triggers (mount,
   * focus regained, route warmed — `watchFocus` below) fire while a tab just sits there
   * open, so a resource that needs "arrives within a minute of a tab that never leaves"
   * has to ask for it explicitly.
   *
   * The delay doubles from `startMs` towards `maxMs` on every tick, and resets the moment
   * `while` turns false, so a later, distinct pending state polls promptly again rather
   * than inheriting the previous one's backoff.
   */
  poll?: PollPolicy<T>;
}

/**
 * A value read before this page's JavaScript ran — by a server component, in the same
 * request — handed to {@link useSeededResource} so the first paint and the hydration agree
 * on real figures instead of a skeleton. `fetchedAt` is epoch ms of the read by the
 * SERVER's clock — diagnostic only: the cache times a seed by when this browser first saw
 * it (see `seed`), because two machines' clocks cannot be ordered against each other.
 */
export interface ResourceSeed<T> {
  data: T;
  fetchedAt: number;
}

/** When to keep polling a mounted entry, and how far apart. See {@link ResourceConfig.poll}. */
export interface PollPolicy<T> {
  while: (data: T | undefined) => boolean;
  startMs: number;
  maxMs: number;
}

/** `useResource`'s private door into a resource's entry map, kept off the public surface. */
const INTERNALS = Symbol("resource.internals");

export interface Resource<T, A extends unknown[]> {
  readonly [INTERNALS]: {
    ensure: (key: string, args: A) => Entry<T>;
    enabled: (...args: A) => boolean;
  };
  readonly name: string;
  /** The cache key these arguments resolve to — the unit `invalidate` and `publish` act on. */
  keyOf: (...args: A) => string;
  /** Cached value if fresh, otherwise a read. Concurrent callers share one request. */
  read: (...args: A) => Promise<T>;
  /** Whatever is cached right now, without subscribing or fetching. */
  peek: (...args: A) => T | undefined;
  /** Warm the cache ahead of a navigation. Fire-and-forget; failures are ignored. */
  prefetch: (...args: A) => void;
  /** Write a value straight in — for a mutation whose response IS the new state. */
  publish: (value: T, ...args: A) => void;
  /**
   * Follow one key's snapshot outside React — for a store or a feed that has to act when a
   * value lands (the chart folds each new tape print into its open bar). The listener
   * counts as a reader, exactly like a mounted screen: a tag invalidation refreshes the
   * key while one is attached. Returns the unsubscribe.
   */
  watch: (listener: (snapshot: ResourceSnapshot<T>) => void, ...args: A) => () => void;
  /** Mark one key stale; refresh it now if a screen is showing it. */
  invalidate: (...args: A) => void;
  /** The same, for every key this resource has cached (all NAVs, all pages). */
  invalidateAll: () => void;
}

interface Entry<T> {
  readonly key: string;
  readonly load: () => Promise<T>;
  readonly freshMs: number;
  readonly tags: readonly string[];
  readonly persistKey: string | null;
  data: T | undefined;
  error: Error | null;
  /**
   * The last attempt was refused outright — see {@link isRefusal}.
   *
   * Suppresses the *automatic* triggers only. `refresh()` and a tag invalidation clear it,
   * because both carry information a clock does not.
   */
  denied: boolean;
  /** 0 until the first successful read — the "never loaded" marker `isStale` reads. */
  fetchedAt: number;
  /**
   * Epoch ms of the last tag invalidation or sign-out. A seed received before it predates
   * the change the invalidation announced, so it is refused — see {@link seed}.
   */
  invalidatedAt: number;
  /**
   * Seed objects already decided on. A view re-renders with the same seed prop for as long
   * as it is mounted, and Back replays it from the router cache; deciding each object once
   * keeps a seed from coming back over a value read after it.
   */
  readonly seeds: WeakSet<object>;
  /** Seeded during a render, which may not notify; the next subscribe tells the others. */
  unannounced: boolean;
  /**
   * From `ResourceConfig.poll`, or null for a resource that only refetches on the clock's
   * other triggers.
   *
   * Held as `PollPolicy<unknown>` rather than `PollPolicy<T>` on purpose: a function taking
   * `T` in a field makes `Entry<T>` invariant in `T`, and the registry — and every sweep
   * that walks it — is typed `Entry<unknown>`. The predicate only ever runs against this
   * entry's own `data`, so the narrowing cast at the single construction site below is
   * where the knowledge that the two agree belongs.
   */
  readonly poll: PollPolicy<unknown> | null;
  /** Backoff delay the NEXT poll tick will wait, in ms. Grows towards `poll.maxMs`; resets to `poll.startMs` once `poll.while` turns false. */
  pollDelayMs: number;
  /** 0 (unarmed) until a poll-eligible tick arms it to `Date.now() + pollDelayMs`; the tick after that one fires. */
  nextPollAt: number;
  inflight: Promise<void> | null;
  readonly listeners: Set<() => void>;
  /** Referentially stable between changes: `useSyncExternalStore` compares it by identity. */
  snapshot: ResourceSnapshot<T>;
  refresh: () => Promise<void>;
}

const REGISTRY = new Map<string, Entry<unknown>>();

const NOOP_REFRESH = () => Promise.resolve();

// Server renders share module scope across every request, so nothing is cached there and no
// entry is created: a client component rendered on the server reports "loading" (or its
// seed, when a server component read one) and picks up the real snapshot when it hydrates. `disabled` is the same shape minus the wait.
const EMPTY_SNAPSHOT = Object.freeze({ data: undefined, error: null, isLoading: true, isValidating: false, refresh: NOOP_REFRESH });
const DISABLED_SNAPSHOT = Object.freeze({ data: undefined, error: null, isLoading: false, isValidating: false, refresh: NOOP_REFRESH });

function emptySnapshot<T>(): ResourceSnapshot<T> {
  return EMPTY_SNAPSHOT as ResourceSnapshot<T>;
}

function disabledSnapshot<T>(): ResourceSnapshot<T> {
  return DISABLED_SNAPSHOT as ResourceSnapshot<T>;
}

function toError(cause: unknown): Error {
  return cause instanceof Error ? cause : new Error(String(cause));
}

function buildSnapshot<T>(entry: Entry<T>): ResourceSnapshot<T> {
  return {
    data: entry.data,
    error: entry.error,
    isLoading: entry.data === undefined && entry.error === null,
    isValidating: entry.inflight !== null,
    refresh: entry.refresh,
  };
}

function publishSnapshot<T>(entry: Entry<T>): void {
  entry.snapshot = buildSnapshot(entry);
  entry.unannounced = false;
  for (const listener of entry.listeners) listener();
}

/**
 * Adopt a value read elsewhere, unless the entry already knows better.
 *
 * Runs during render (it has to: the first `getSnapshot` must already see it), so it
 * writes the snapshot without notifying — telling another component to update while this
 * one renders is an error. `unannounced` defers that to the next subscribe.
 *
 * Decided once per seed object, at the moment this browser first sees it, and timed by
 * this browser's clock alone: `value.fetchedAt` is the server's clock, and comparing it
 * with times taken here lets any skew between the two machines reorder events — a server
 * running ahead made a pre-withdrawal balance outrank the read that followed the
 * withdrawal. Refused when received no later than the last invalidation or than the value
 * the entry already holds.
 */
function seed<T>(entry: Entry<T>, value: ResourceSeed<T>): void {
  if (entry.seeds.has(value)) return;
  entry.seeds.add(value);
  const fetchedAt = Date.now();
  if (fetchedAt <= entry.invalidatedAt) return;
  if (entry.data !== undefined && entry.fetchedAt >= fetchedAt) return;
  entry.data = value.data;
  entry.error = null;
  entry.denied = false;
  entry.fetchedAt = fetchedAt;
  writePersisted(entry);
  entry.snapshot = buildSnapshot(entry);
  entry.unannounced = true;
}

function isStale(entry: Entry<unknown>): boolean {
  return entry.fetchedAt === 0 || Date.now() - entry.fetchedAt >= entry.freshMs;
}

/**
 * A verdict rather than a blip: this caller may not read this, and asking again changes
 * nothing.
 *
 * 403 alone. A 401 is already healed and replayed a layer down (`./api-client.ts`), a 404
 * can become a 200 the moment the thing is created, a 429 is explicitly an invitation to
 * come back, and 5xx and a dead network are the transient cases the retry loop exists for.
 * Only "you do not have access to this" is settled by information the client cannot change
 * on its own.
 *
 * Read off the `status` field rather than `instanceof RequestError`, and both halves of
 * that are deliberate. This module has no imports but React — it is the cache, so anything
 * it pulls in is pulled into every screen — and importing the transport would drag
 * `fetch`, the CSRF header and the session into it. It would also break `./resource.
 * test.ts`: the Node runner strips types but resolves no `@/` alias, so a value import
 * from `@/shared/lib/api-client` is `ERR_MODULE_NOT_FOUND` (`session.ts` gets away with
 * the same alias only because its import is type-only). The shape checked here is the one
 * `RequestError` and `SessionExpiredError` both publish, and it is narrowed rather than
 * asserted.
 */
function isRefusal(error: Error): boolean {
  const status: unknown = (error as { status?: unknown }).status;
  return status === 403;
}

function revalidate<T>(entry: Entry<T>): Promise<void> {
  if (entry.inflight) return entry.inflight;
  entry.inflight = entry
    .load()
    .then((value) => {
      entry.data = value;
      entry.error = null;
      entry.denied = false;
      entry.fetchedAt = Date.now();
      writePersisted(entry);
    })
    .catch((cause: unknown) => {
      // Deliberately non-destructive: the stale value stays, so a timed-out poll never
      // blanks a figure the user is reading.
      entry.error = toError(cause);
      entry.denied = isRefusal(entry.error);
    })
    .finally(() => {
      entry.inflight = null;
      publishSnapshot(entry);
    });
  publishSnapshot(entry);
  return entry.inflight;
}

/**
 * The clock's way in — a mount, a tab regaining focus, a route being warmed. (The poll
 * sweep in `watchPoll` below is a fourth trigger with the same shape; it checks `denied`
 * itself rather than routing through here, since it also owns its own backoff schedule.)
 *
 * None of these know anything the last attempt did not, so a refused entry is left alone:
 * an operator on the users screen was re-issuing `/api/owners` on every one of them and
 * collecting an identical 403 in the console each time. The refusal is still reported —
 * `snapshot.error` holds it and the screens branch on it — it is only the *repeat* that is
 * dropped. `refresh()` and `revalidateTag()` go through `revalidate` directly, because a
 * retry the reader asked for and a mutation saying "this changed" are both new information.
 */
function autoRevalidate(entry: Entry<unknown>): void {
  if (entry.denied || !isStale(entry)) return;
  void revalidate(entry);
}

function writePersisted<T>(entry: Entry<T>): void {
  if (!entry.persistKey) return;
  try {
    sessionStorage.setItem(entry.persistKey, JSON.stringify({ v: entry.data, t: entry.fetchedAt }));
  } catch {
    // Private mode, quota, disabled storage — the in-memory cache is unaffected.
  }
}

function readPersisted<T>(entry: Entry<T>): void {
  if (!entry.persistKey) return;
  try {
    const raw = sessionStorage.getItem(entry.persistKey);
    if (!raw) return;
    const stored = JSON.parse(raw) as { v: T; t: number };
    entry.data = stored.v;
    entry.fetchedAt = stored.t;
    publishSnapshot(entry);
  } catch {
    // Unreadable or written by an older shape — fall through to a live read.
  }
}

function dropPersisted(entry: Entry<unknown>): void {
  if (!entry.persistKey) return;
  try {
    sessionStorage.removeItem(entry.persistKey);
  } catch {
    /* nothing to drop */
  }
}

/**
 * Tell the readers that rendered before a seed landed. The snapshot object is already the
 * seeded one, so the component that seeded compares equal and skips this.
 */
function announceSeed(entry: Entry<unknown>): void {
  if (!entry.unannounced) return;
  entry.unannounced = false;
  for (const listener of entry.listeners) listener();
}

function markStale(entry: Entry<unknown>): void {
  entry.fetchedAt = 0;
  entry.invalidatedAt = Date.now();
  // A named tag is a mutation or the server's own stream saying this changed, which is
  // exactly the kind of event that turns a 403 into a 200 — a seat granted, a role moved.
  // So it lifts the refusal, unlike the clock (see `autoRevalidate`).
  entry.denied = false;
  dropPersisted(entry);
  // On screen right now: refresh in place, so a mutation's effect lands without the user
  // navigating. Not mounted: the next screen that reads it will find it stale and refresh.
  if (entry.listeners.size > 0) void revalidate(entry);
}

/**
 * Mark every cached read carrying any of these tags stale — the counterpart to Next's
 * `revalidateTag`, and how a mutation says what it changed. A withdrawal moves the balance,
 * the withdrawal list and the activity timeline; naming those three tags is the whole
 * contract, and no call site has to know which screens are currently mounted.
 */
export function revalidateTag(...tags: readonly string[]): void {
  for (const entry of REGISTRY.values()) {
    if (tags.some((tag) => entry.tags.includes(tag))) markStale(entry);
  }
}

/**
 * Drop every cached value. For sign-out: the next account to use this tab must never be
 * shown a figure belonging to the previous one. Entries are reset in place rather than
 * removed, so a component still mounted mid-teardown re-renders empty instead of holding a
 * reference to an orphaned entry.
 */
export function clearResources(): void {
  for (const entry of REGISTRY.values()) {
    dropPersisted(entry);
    entry.data = undefined;
    entry.error = null;
    // The next account in this tab is a different caller, so the previous one's refusal
    // says nothing about them.
    entry.denied = false;
    entry.fetchedAt = 0;
    entry.invalidatedAt = Date.now();
    publishSnapshot(entry);
  }
}

// Coming back to a tab that has been in the background is exactly when the numbers on it
// are most likely to be old — and a backgrounded tab's timers are throttled, so nothing
// else would have caught it. Only mounted, stale entries are refreshed, so returning to a
// tab costs at most one read per figure actually on screen.
let watchingFocus = false;

function watchFocus(): void {
  if (watchingFocus || typeof document === "undefined") return;
  watchingFocus = true;
  const sweep = () => {
    if (document.visibilityState !== "visible") return;
    for (const entry of REGISTRY.values()) {
      if (entry.listeners.size > 0) autoRevalidate(entry);
    }
  };
  document.addEventListener("visibilitychange", sweep);
  window.addEventListener("online", sweep);
}

/** Smallest gap between two checks of the same polling entry — the sweep's own clock. */
const POLL_TICK_MS = 5_000;

// One timer for every polling entry in the cabinet, not one per entry: `poll` is rare
// (one resource, at the time of writing) and a single low-frequency sweep costs nothing
// idle, whereas N independent `setInterval`s would each need their own drift-free
// rescheduling. `nextPollAt` is what makes this a per-entry backoff despite the shared
// clock: a tick that finds an entry not yet due, hidden, or unmounted just skips it.
let watchingPoll = false;

function watchPoll(): void {
  if (watchingPoll || typeof document === "undefined") return;
  watchingPoll = true;
  setInterval(() => pollSweep(Date.now(), document.visibilityState === "visible"), POLL_TICK_MS);
}

/**
 * One pass of the poll clock over every entry in the registry.
 *
 * Split out of the timer because `now` and `visible` are the only two things the sweep
 * reads from the outside world: given both, the whole policy — arm before firing, double
 * the delay, pause while hidden, rearm once `while` closes — is exercisable against an
 * explicit clock instead of POLL_TICK_MS of real time.
 */
function pollSweep(now: number, visible: boolean): void {
  for (const entry of REGISTRY.values()) {
    const poll = entry.poll;
    if (!poll || entry.listeners.size === 0) continue;
    if (!poll.while(entry.data)) {
      // Closed (or never opened): rearm from the start so the next pending state — a
      // new case — doesn't inherit this one's backoff.
      entry.pollDelayMs = poll.startMs;
      entry.nextPollAt = 0;
      continue;
    }
    if (entry.nextPollAt === 0) {
      // Just became eligible. Arm rather than fire immediately: the mount that made it
      // eligible already triggered its own read via `autoRevalidate`.
      entry.nextPollAt = now + entry.pollDelayMs;
      continue;
    }
    // A backgrounded tab pauses rather than burning its backoff budget unseen —
    // `watchFocus` is what catches it up the moment it regains focus.
    if (entry.denied || now < entry.nextPollAt || !visible) continue;
    entry.pollDelayMs = Math.min(entry.pollDelayMs * 2, poll.maxMs);
    entry.nextPollAt = now + entry.pollDelayMs;
    void revalidate(entry);
  }
}

export function defineResource<T, A extends unknown[] = []>(config: ResourceConfig<T, A>): Resource<T, A> {
  const freshMs = (config.revalidate ?? DEFAULT_REVALIDATE_S) * 1000;
  const tags = config.tags ?? [];

  const keyOf = (...args: A): string => `${config.name}(${config.key ? config.key(...args) : ""})`;

  function ensure(key: string, args: A): Entry<T> {
    const existing = REGISTRY.get(key) as Entry<T> | undefined;
    if (existing) return existing;
    const entry: Entry<T> = {
      key,
      load: () => config.fetch(...args),
      freshMs,
      tags,
      persistKey: config.persist ? `${PERSIST_PREFIX}${key}` : null,
      data: undefined,
      error: null,
      denied: false,
      fetchedAt: 0,
      invalidatedAt: 0,
      seeds: new WeakSet(),
      unannounced: false,
      // Sound because the sweep only ever hands it `entry.data`, which is this `T`.
      poll: (config.poll as PollPolicy<unknown> | undefined) ?? null,
      pollDelayMs: config.poll?.startMs ?? 0,
      nextPollAt: 0,
      inflight: null,
      listeners: new Set(),
      snapshot: emptySnapshot<T>(),
      refresh: NOOP_REFRESH,
    };
    // The reader asked, so it goes through regardless of the last verdict: a retry button
    // that silently does nothing is worse than a second 403.
    entry.refresh = () => {
      entry.fetchedAt = 0;
      entry.denied = false;
      return revalidate(entry);
    };
    REGISTRY.set(key, entry as Entry<unknown>);
    readPersisted(entry);
    publishSnapshot(entry);
    watchFocus();
    if (entry.poll) watchPoll();
    return entry;
  }

  const enabled = (...args: A) => config.enabled?.(...args) ?? true;

  return {
    [INTERNALS]: { ensure, enabled },
    name: config.name,
    keyOf,
    read(...args: A): Promise<T> {
      const entry = ensure(keyOf(...args), args);
      if (!isStale(entry) && entry.data !== undefined) return Promise.resolve(entry.data);
      // Refused, and nothing has happened since that could change the answer — hand back
      // the verdict already held rather than spending a request on the same one.
      if (entry.denied && entry.error !== null) return Promise.reject(entry.error);
      return revalidate(entry).then(() => {
        if (entry.data !== undefined) return entry.data;
        throw entry.error ?? new Error(`${config.name} unavailable`);
      });
    },
    peek(...args: A): T | undefined {
      return REGISTRY.get(keyOf(...args))?.data as T | undefined;
    },
    prefetch(...args: A): void {
      if (typeof window === "undefined" || !enabled(...args)) return;
      autoRevalidate(ensure(keyOf(...args), args));
    },
    publish(value: T, ...args: A): void {
      const entry = ensure(keyOf(...args), args);
      entry.data = value;
      entry.error = null;
      entry.denied = false;
      entry.fetchedAt = Date.now();
      writePersisted(entry);
      publishSnapshot(entry);
    },
    watch(listener, ...args: A): () => void {
      if (typeof window === "undefined" || !enabled(...args)) return () => undefined;
      const entry = ensure(keyOf(...args), args);
      const fn = () => listener(entry.snapshot);
      entry.listeners.add(fn);
      return () => {
        entry.listeners.delete(fn);
      };
    },
    invalidate(...args: A): void {
      const entry = REGISTRY.get(keyOf(...args));
      if (entry) markStale(entry);
    },
    invalidateAll(): void {
      const prefix = `${config.name}(`;
      for (const entry of REGISTRY.values()) {
        if (entry.key.startsWith(prefix)) markStale(entry);
      }
    },
  };
}

/**
 * Read a resource, re-rendering when its value changes.
 *
 * Returns cached data on the first render — that is the point. A screen revisited inside
 * the same session paints its real figures immediately and refreshes them behind itself;
 * only a genuinely cold read reports `isLoading`.
 */
export function useResource<T, A extends unknown[]>(resource: Resource<T, A>, ...args: A): ResourceSnapshot<T> {
  return useSeededResource(resource, undefined, ...args);
}

/**
 * {@link useResource}, starting from a value a server component already read.
 *
 * The server renders `seed` instead of a skeleton, and hydration reads the same `seed`, so
 * the two agree. In the browser the seed is written into the shared entry — unless the
 * entry holds something newer (see {@link seed}) — so every other screen reading this key
 * starts from it too; revalidation, polling and tag invalidation then run as for any
 * browser read. Without a seed this is exactly `useResource`.
 */
export function useSeededResource<T, A extends unknown[]>(resource: Resource<T, A>, initial: ResourceSeed<T> | undefined, ...args: A): ResourceSnapshot<T> {
  const internals = resource[INTERNALS];
  const key = resource.keyOf(...args);
  const active = internals.enabled(...args);
  // Creating the entry during render is a lookup-or-insert into a module map: idempotent,
  // observable by nobody, and it never starts a request — subscribing does. Doing it here
  // is what gives `getSnapshot` a stable entry to read on the very first render, which is
  // the difference between painting cached data and painting a skeleton. Skipped on the
  // server, where module scope is shared between users.
  const entry = typeof window === "undefined" || !active ? null : internals.ensure(key, args);
  if (entry && initial) seed(entry, initial);

  const subscribe = useCallback(
    (onChange: () => void) => {
      if (!entry) return () => undefined;
      entry.listeners.add(onChange);
      announceSeed(entry);
      autoRevalidate(entry);
      return () => {
        entry.listeners.delete(onChange);
      };
    },
    [entry],
  );

  // Per seed object, so the server and hydration renders hand React the same snapshot.
  const seeded = useMemo(() => (initial && active ? seededSnapshot(initial) : null), [initial, active]);
  const fallback = useCallback((): ResourceSnapshot<T> => seeded ?? (active ? emptySnapshot<T>() : disabledSnapshot<T>()), [seeded, active]);
  return useSyncExternalStore(subscribe, () => entry?.snapshot ?? fallback(), fallback);
}

function seededSnapshot<T>(initial: ResourceSeed<T>): ResourceSnapshot<T> {
  return Object.freeze({ data: initial.data, error: null, isLoading: false, isValidating: false, refresh: NOOP_REFRESH });
}

/** Test seam: forget every cached value AND every entry, so each case starts cold. */
export function resetResourcesForTests(): void {
  REGISTRY.clear();
}

/**
 * Test seam: pretend a screen is showing this key. Returns the unmount.
 *
 * The poll sweep skips entries nobody is looking at, which is the point of it — so a test
 * about polling has to be able to say "this one is on screen" without a React renderer.
 */
export function mountForTests<T, A extends unknown[]>(resource: Resource<T, A>, ...args: A): () => void {
  const entry = resource[INTERNALS].ensure(resource.keyOf(...args), args);
  const listener = () => {};
  entry.listeners.add(listener);
  return () => {
    entry.listeners.delete(listener);
  };
}

/** Test seam: run one pass of the poll clock at an explicit time, with the tab visible or not. */
export function pollSweepForTests(now: number, visible = true): void {
  pollSweep(now, visible);
}
