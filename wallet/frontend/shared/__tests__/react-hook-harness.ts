// A React hook under Node's built-in test runner, with no DOM library.
//
// The cabinet's tests run on `node --test`, and a hook needs a renderer for its effects to
// run. The real `react-dom/client` is enough when the probe component renders nothing: the
// root container is only asked for a few properties, which the stand-in below provides.
// What a hook does to the page it does through the host element and globals the test hands
// it (see `fake-chart-engine.ts`), so nothing here pretends to be a browser beyond that.
//
// `installModuleHooks` registers `module-hooks.mjs`: modules loaded AFTER it may use the `@/`
// alias, `lightweight-charts` is the fake engine, and the stubs named resolve to their
// stand-ins. Load the module under test with a dynamic `import()` for that reason — and with
// a unique query, to get a fresh module-level cache per test.

import { register } from "node:module";
import { createElement, act } from "react";
import { createRoot } from "react-dom/client";

/** Registers the module hooks; `stubs` maps a specifier to the file URL that replaces it. */
export function installModuleHooks(stubs: Record<string, string> = {}): void {
  register("./module-hooks.mjs", { parentURL: import.meta.url, data: { stubs } });
}

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
// React DOM reads `window.event` to pick an update's priority; nothing else of `window` is
// touched by a root that renders no host elements.
(globalThis as Record<string, unknown>).window ??= { event: undefined };

function rootContainer(): Element {
  const ignore = () => undefined;
  const doc: Record<string, unknown> = { nodeType: 9, addEventListener: ignore, removeEventListener: ignore, activeElement: null, body: null };
  doc.defaultView = { document: doc, HTMLIFrameElement: class {} };
  // A stand-in, not an Element: these are the only members React DOM reads off a root
  // container whose tree never mounts a host node.
  return { nodeType: 1, nodeName: "DIV", tagName: "DIV", namespaceURI: "http://www.w3.org/1999/xhtml", ownerDocument: doc, addEventListener: ignore, removeEventListener: ignore, textContent: "" } as unknown as Element;
}

export interface RenderedHook<P, R> {
  /** The value the hook returned on its latest render. */
  readonly current: R;
  /** Every value the hook returned, render by render. */
  readonly history: readonly R[];
  rerender(props: P): Promise<void>;
  unmount(): Promise<void>;
}

/** Mounts `useHook(props)` in a probe component and flushes its effects. */
export async function renderHook<P, R>(useHook: (props: P) => R, props: P): Promise<RenderedHook<P, R>> {
  const history: R[] = [];
  function Probe(p: { props: P }) {
    history.push(useHook(p.props));
    return null;
  }
  const root = createRoot(rootContainer());
  await act(async () => root.render(createElement(Probe, { props })));
  return {
    get current() {
      if (history.length === 0) throw new Error("the hook never rendered");
      return history[history.length - 1];
    },
    history,
    rerender: (next) => act(async () => root.render(createElement(Probe, { props: next }))),
    unmount: () => act(async () => root.unmount()),
  };
}

/**
 * Runs `step` inside `act`, awaits it, and lets every promise it set off settle before React
 * flushes — one macrotask turn, which drains the microtask queue. Not a timer: nothing here
 * waits for time to pass, only for already-scheduled continuations to run.
 */
export async function settle(step: () => unknown = () => undefined): Promise<void> {
  await act(async () => {
    await step();
    await new Promise<void>((resolve) => setImmediate(resolve));
  });
}

/** A promise the test resolves or rejects by hand. */
export interface Deferred<T> {
  promise: Promise<T>;
  resolve(value: T): void;
  reject(error: unknown): void;
}

export function deferred<T>(): Deferred<T> {
  return Promise.withResolvers<T>();
}
