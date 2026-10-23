// The controller behind `fake-lightweight-charts.mjs`: decides when each engine download
// lands (or fails) and records what the hook under test did with the engine — charts
// created and removed, data set on each series, resize observers still attached.

const KEY = Symbol.for("cabinet.test.chartEngine");

export interface FakeSeries {
  kind: string;
  /** Every `setData` payload, in order. */
  data: unknown[][];
  /** Every `update` payload, in order. */
  updates: unknown[];
  options: Record<string, unknown>;
}

export interface FakeChart {
  host: unknown;
  series: FakeSeries[];
  removed: boolean;
}

interface Download {
  resolve: () => void;
  reject: (error: unknown) => void;
}

export interface FakeChartEngine {
  /** Downloads started so far (one per `import("lightweight-charts")` that missed the cache). */
  readonly downloads: number;
  readonly charts: readonly FakeChart[];
  /** Resize observers still observing — a leak if a chart was torn down. */
  readonly observing: number;
  /** Lets the oldest pending download land — waiting, if need be, for the hook to start it
   *  (an `import()` reaches the fake module a few event-loop turns after it is called). */
  arrive(): Promise<void>;
  /** Fails the oldest pending download, waiting for it to start as `arrive` does. */
  fail(error: unknown): Promise<void>;
}

/**
 * A fake engine for the module under test loaded as `…?case=<forCase>`: only imports made
 * from that copy of the module reach this controller.
 */
export function fakeChartEngine(forCase: string): FakeChartEngine {
  const pending: Download[] = [];
  // Callers of `next` that asked before any download had started, oldest first.
  const waiting: ((download: Download) => void)[] = [];
  const charts: FakeChart[] = [];
  let downloads = 0;
  let observing = 0;

  const next = (): Promise<Download> => {
    const download = pending.shift();
    return download ? Promise.resolve(download) : new Promise((resolve) => waiting.push(resolve));
  };

  const registry = ((globalThis as Record<symbol, Map<string, unknown> | undefined>)[KEY] ??= new Map());
  registry.set(forCase, {
    download(): Promise<void> {
      downloads += 1;
      return new Promise<void>((resolve, reject) => {
        const download = { resolve, reject };
        const waiter = waiting.shift();
        if (waiter) waiter(download);
        else pending.push(download);
      });
    },
    createChart(host: unknown): unknown {
      const chart: FakeChart = { host, series: [], removed: false };
      charts.push(chart);
      return {
        addSeries(definition: { kind: string }, options: Record<string, unknown> = {}) {
          const series: FakeSeries = { kind: definition.kind, data: [], updates: [], options: { ...options } };
          chart.series.push(series);
          return {
            setData: (points: unknown[]) => void series.data.push(points),
            update: (point: unknown) => void series.updates.push(point),
            applyOptions: (patch: Record<string, unknown>) => void Object.assign(series.options, patch),
          };
        },
        applyOptions: () => undefined,
        resize: () => undefined,
        timeScale: () => ({ fitContent: () => undefined }),
        remove: () => {
          chart.removed = true;
        },
      };
    },
  });

  (globalThis as Record<string, unknown>).ResizeObserver = class {
    #on = false;
    observe() {
      if (!this.#on) observing += 1;
      this.#on = true;
    }
    disconnect() {
      if (this.#on) observing -= 1;
      this.#on = false;
    }
    unobserve() {
      this.disconnect();
    }
  };

  return {
    get downloads() {
      return downloads;
    },
    get charts() {
      return charts;
    },
    get observing() {
      return observing;
    },
    arrive: async () => (await next()).resolve(),
    fail: async (error) => (await next()).reject(error),
  };
}
