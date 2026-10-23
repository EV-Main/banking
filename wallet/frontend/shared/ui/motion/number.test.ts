// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Pins the browser half of `AnimatedNumber` — the deps it hands `driveNumber` — against the
// real `animateValue` from motion. `number-driver.test.ts` covers the driver with a fake
// engine; what it cannot see is the wiring to the real one: `animateValue` takes
// MILLISECONDS where the `animate(from, to, …)` it replaced took seconds, and the driver
// relies on the object it returns having a working `stop()`.
//
// The component is mounted with the real `react-dom/client` into a minimal element tree
// (below), and motion's frame loop runs on a hand-cranked clock: `performance.now` and
// `requestAnimationFrame` are the test's, so "200 ms later" is a statement, not a wait.
import assert from "node:assert/strict";
import test from "node:test";

import { createElement, act } from "react";
import { createRoot } from "react-dom/client";

import { installModuleHooks } from "../../__tests__/react-hook-harness.ts";

// `number.tsx` imports its siblings without an extension, the way the bundler resolves them.
installModuleHooks({
  "./number-driver": new URL("./number-driver.ts", import.meta.url).href,
  "./tokens": new URL("./tokens.ts", import.meta.url).href,
});

// ---- A clock and a frame loop the test drives ----------------------------------------------

let clock = 0;
let frameQueue: FrameRequestCallback[] = [];
Object.defineProperty(performance, "now", { value: () => clock, configurable: true });
// Motion picks its frame scheduler when it is first loaded, so this is in place before the
// dynamic import of the module under test below.
(globalThis as Record<string, unknown>).requestAnimationFrame = (cb: FrameRequestCallback) => {
  frameQueue.push(cb);
  return frameQueue.length;
};
(globalThis as Record<string, unknown>).cancelAnimationFrame = () => undefined;

/** Moves the clock to `ms` and runs the frame that was requested for it. */
function frameAt(ms: number): void {
  clock = ms;
  const due = frameQueue;
  frameQueue = [];
  for (const cb of due) cb(ms);
}

// ---- The smallest element tree react-dom/client renders a <span>{text}</span> into ---------

interface FakeText {
  nodeType: 3;
  nodeValue: string;
  parentNode: FakeElement | null;
}

class FakeElement {
  readonly nodeType = 1;
  readonly namespaceURI = "http://www.w3.org/1999/xhtml";
  readonly style: Record<string, string> = {};
  readonly childNodes: Array<FakeElement | FakeText> = [];
  parentNode: FakeElement | null = null;
  readonly tagName: string;
  readonly ownerDocument: unknown;
  constructor(tagName: string, ownerDocument: unknown) {
    this.tagName = tagName;
    this.ownerDocument = ownerDocument;
  }
  get nodeName(): string {
    return this.tagName;
  }
  get firstChild() {
    return this.childNodes[0] ?? null;
  }
  get lastChild() {
    return this.childNodes[this.childNodes.length - 1] ?? null;
  }
  get textContent(): string {
    return this.childNodes.map((n) => (n.nodeType === 3 ? n.nodeValue : n.textContent)).join("");
  }
  set textContent(text: string) {
    this.childNodes.length = 0;
    if (text) this.childNodes.push({ nodeType: 3, nodeValue: text, parentNode: this });
  }
  appendChild<T extends FakeElement | FakeText>(child: T): T {
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
  }
  insertBefore<T extends FakeElement | FakeText>(child: T, before: FakeElement | FakeText | null): T {
    child.parentNode = this;
    const at = before ? this.childNodes.indexOf(before) : -1;
    if (at < 0) this.childNodes.push(child);
    else this.childNodes.splice(at, 0, child);
    return child;
  }
  removeChild<T extends FakeElement | FakeText>(child: T): T {
    this.childNodes.splice(this.childNodes.indexOf(child), 1);
    child.parentNode = null;
    return child;
  }
  setAttribute(): void {}
  removeAttribute(): void {}
  addEventListener(): void {}
  removeEventListener(): void {}
}

const ignore = () => undefined;
const fakeDocument: Record<string, unknown> = {
  nodeType: 9,
  visibilityState: "visible",
  activeElement: null,
  body: null,
  addEventListener: ignore,
  removeEventListener: ignore,
};
fakeDocument.createElement = (tag: string) => new FakeElement(tag.toUpperCase(), fakeDocument);
fakeDocument.defaultView = { document: fakeDocument, HTMLIFrameElement: class {} };
// `AnimatedNumber` reads `document.visibilityState` and `Node.TEXT_NODE`; react-dom keeps to
// the container's `ownerDocument`.
(globalThis as Record<string, unknown>).document = fakeDocument;
(globalThis as Record<string, unknown>).Node = { TEXT_NODE: 3 };
// Read by motion's `animate(from, to, …)` path while it decides between WAAPI and the frame
// loop; nothing is an instance of them, so a bare number takes the frame loop as in a browser.
// Not needed by `animateValue`, but they keep these tests about the count, not about which
// entry point drives it.
(globalThis as Record<string, unknown>).HTMLElement = class {};
(globalThis as Record<string, unknown>).SVGElement = class {};

const { AnimatedNumber } = await import("./number.tsx");

const format = (n: number) => `$${n.toFixed(2)}`;
const figureOf = (text: string) => Number(text.replace(/[^0-9.]/g, ""));

/**
 * Mounts `<AnimatedNumber value={value} />` and returns its frame crank, relative to the
 * mount: motion's frame loop is module state shared by every test, so the clock only moves
 * forward and each count starts well clear of the previous one.
 */
async function mount(value: number) {
  const t0 = clock + 10_000;
  clock = t0;
  const container = new FakeElement("DIV", fakeDocument);
  const root = createRoot(container as unknown as Element);
  await act(async () => root.render(createElement(AnimatedNumber, { value, format })));
  const span = container.firstChild as FakeElement;
  return {
    frame: (ms: number) => frameAt(t0 + ms),
    text: () => span.textContent,
    unmount: () => act(async () => root.unmount()),
  };
}

test("first appearance counts up from zero along the 400 ms ease-out curve", async () => {
  const figure = await mount(1000);

  figure.frame(0);
  figure.frame(100);

  // cubic-bezier(0.22, 1, 0.36, 1) a quarter of the way through puts the count at 764.86
  // (reference curve, bisection to 1e-9); motion's solver lands within a few cents of it.
  // The window rules out every neighbour: linear 250, the 280 ms duration 887.26, the
  // in-out curve 70.80, and a duration misread by 1000× either way 1000 or 0.
  const shown = figureOf(figure.text());
  assert.ok(shown > 755 && shown < 775, `expected ≈ $764.86 100 ms into the count, got ${figure.text()}`);

  await figure.unmount();
});

test("the count finishes by itself at 400 ms, before the 650 ms fallback", async () => {
  const figure = await mount(1000);

  figure.frame(0);
  figure.frame(200);
  const midway = figure.text();
  figure.frame(400);

  assert.notEqual(midway, "$1000.00", "a duration of 0.4 ms (seconds passed as ms) finishes by 200 ms");
  assert.equal(figure.text(), "$1000.00", "the animation itself must land the figure at 400 ms");

  await figure.unmount();
});

test("unmounting mid-count stops the animation: it asks for no further frames", async () => {
  const figure = await mount(1000);
  figure.frame(0);
  figure.frame(100);

  await figure.unmount();
  // Still mid-count by the clock: an animation that kept running would request the next
  // frame here. (By 400 ms it would have finished and stopped asking on its own.)
  figure.frame(200);

  assert.equal(frameQueue.length, 0, "a stopped count requests no further animation frames");
});
