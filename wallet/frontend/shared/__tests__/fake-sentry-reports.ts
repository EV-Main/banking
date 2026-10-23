// The test's end of `fake-sentry.mjs`. The report is sent from a lazily imported SDK, a few
// event-loop turns after the failure, so a test waits for it rather than for time.

const KEY = Symbol.for("cabinet.test.sentry");

export interface SentReport {
  error: unknown;
  hint: unknown;
}

function store(): { reports: SentReport[]; waiters: (() => void)[] } {
  return ((globalThis as Record<symbol, { reports: SentReport[]; waiters: (() => void)[] } | undefined>)[KEY] ??= { reports: [], waiters: [] });
}

/** Everything captured so far, oldest first. */
export const sentReports = (): readonly SentReport[] => store().reports;

/** Resolves once at least `count` reports have been captured. */
export function reportsReach(count: number): Promise<void> {
  if (store().reports.length >= count) return Promise.resolve();
  return new Promise<void>((resolve) => store().waiters.push(() => void reportsReach(count).then(resolve)));
}
