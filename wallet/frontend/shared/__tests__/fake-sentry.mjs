// Stand-in for `@sentry/react` (via `module-hooks.mjs`, `stubs`): records what would have been
// sent, and wakes whoever waits on `reportsReach()` from `fake-sentry-reports.ts`.

const KEY = Symbol.for("cabinet.test.sentry");
const store = (globalThis[KEY] ??= { reports: [], waiters: [] });

export function captureException(error, hint) {
  store.reports.push({ error, hint });
  store.waiters.splice(0).forEach((wake) => wake());
}
