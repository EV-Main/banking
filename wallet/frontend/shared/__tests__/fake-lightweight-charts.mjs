// Stand-in for the `lightweight-charts` chunk, served by `module-hooks.mjs`. Each import is
// one "download": it parks on the controller from `fake-chart-engine.ts` until the test
// lets it arrive or fail, and every chart it creates is recorded there. The controller is
// the one registered for the importer's `?case=` (see `module-hooks.mjs`).

const forCase = new URL(import.meta.url).searchParams.get("case") ?? "";
const control = globalThis[Symbol.for("cabinet.test.chartEngine")]?.get(forCase);
if (!control) throw new Error(`fake-lightweight-charts: no controller for case "${forCase}" — call fakeChartEngine(case) before importing the hook`);

await control.download();

export const ColorType = { Solid: "solid" };
export const CandlestickSeries = { kind: "Candlestick" };
export const LineSeries = { kind: "Line" };

export function createChart(host, options) {
  return control.createChart(host, options);
}
