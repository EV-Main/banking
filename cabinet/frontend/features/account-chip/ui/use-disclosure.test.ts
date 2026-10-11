// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// `useDisclosure` keeps the chip's panel painted while it animates out, then hides it. What
// is pinned here: opening styles the panel once in its closed look before it opens (one
// forced layout read, or nothing transitions); closing ends on the panel's own opacity
// `transitionend`; when that event never comes, a timer sized from the panel's computed
// transitions still ends it; and reopening mid-exit reverses into `open` without a second
// enter and without the stale timer closing it afterwards.
//
// No DOM library is installed. react-dom's client renderer runs here against the few
// globals it reads when the tree renders nothing (`window`, `document.activeElement`,
// `HTMLIFrameElement`); the panel is a plain EventTarget behind the ref, which is all the
// hook touches besides `getComputedStyle` and `window.setTimeout`.
import assert from "node:assert/strict";
import test, { after, afterEach, before, beforeEach, mock } from "node:test";

import { act, createElement, type RefObject } from "react";
import { createRoot, type Root } from "react-dom/client";

import { useDisclosure, type DisclosurePhase } from "./use-disclosure.ts";

class FakePanel extends EventTarget {
  layoutReads = 0;
  transitionDuration = "0.15s, 0.2s";
  transitionDelay = "0s, 0.05s";
  getBoundingClientRect(): DOMRect {
    this.layoutReads += 1;
    return {} as DOMRect;
  }
}

/** A `transitionend` as the browser fires it, with `target` set to where it ran. */
function transitionEnd(on: FakePanel, propertyName: string, target: EventTarget = on): void {
  const event = Object.assign(new Event("transitionend"), { propertyName });
  Object.defineProperty(event, "target", { value: target });
  on.dispatchEvent(event);
}

const GLOBALS = ["window", "document", "HTMLIFrameElement", "getComputedStyle", "IS_REACT_ACT_ENVIRONMENT"] as const;

before(() => {
  Reflect.set(globalThis, "IS_REACT_ACT_ENVIRONMENT", true);
  Reflect.set(globalThis, "window", globalThis);
  Reflect.set(globalThis, "document", Object.assign(new EventTarget(), { activeElement: null }));
  Reflect.set(globalThis, "HTMLIFrameElement", class {});
  Reflect.set(globalThis, "getComputedStyle", (el: FakePanel) => ({
    transitionDuration: el.transitionDuration,
    transitionDelay: el.transitionDelay,
  }));
});

after(() => {
  for (const name of GLOBALS) Reflect.deleteProperty(globalThis, name);
});

let panel: FakePanel;
let root: Root;
/** Every phase the probe rendered, in order. */
let phases: DisclosurePhase[];
let disclosure: Disclosure;

type Disclosure = ReturnType<typeof useDisclosure>;

/** Reports every render's disclosure to the test, which keeps the latest and the phase log. */
function Probe({ panelRef, onRender }: { panelRef: RefObject<HTMLElement | null>; onRender: (d: Disclosure) => void }) {
  onRender(useDisclosure(panelRef));
  return null;
}

function record(d: Disclosure): void {
  disclosure = d;
  phases.push(d.phase);
}

beforeEach(async () => {
  mock.timers.enable({ apis: ["setTimeout"] });
  panel = new FakePanel();
  phases = [];
  const container = Object.assign(new EventTarget(), {
    nodeType: 1,
    nodeName: "DIV",
    tagName: "DIV",
    namespaceURI: "http://www.w3.org/1999/xhtml",
    ownerDocument: globalThis.document,
  });
  // The hook only reads the panel through its ref; the cast is the whole of the stand-in.
  const panelRef = { current: panel as unknown as HTMLElement };
  root = createRoot(container as unknown as Element);
  await act(() => root.render(createElement(Probe, { panelRef, onRender: record })));
});

afterEach(async () => {
  await act(() => root.unmount());
  mock.timers.reset();
});

async function openPanel(): Promise<void> {
  await act(() => disclosure.open());
}

async function closePanel(): Promise<void> {
  await act(() => disclosure.close());
}

test("opening renders the closed look once, reads layout, then opens", async () => {
  await openPanel();

  assert.deepEqual(phases, ["closed", "entering", "open"]);
  assert.equal(panel.layoutReads, 1);
});

test("closing keeps the panel painted until its opacity transition ends", async () => {
  await openPanel();
  await closePanel();
  assert.equal(disclosure.phase, "closing");

  await act(() => transitionEnd(panel, "opacity"));

  assert.equal(disclosure.phase, "closed");
});

test("another property's transitionend does not end the close", async () => {
  await openPanel();
  await closePanel();

  await act(() => transitionEnd(panel, "transform"));

  assert.equal(disclosure.phase, "closing");
});

test("a child's opacity transitionend bubbling up does not end the close", async () => {
  await openPanel();
  await closePanel();

  await act(() => transitionEnd(panel, "opacity", new EventTarget()));

  assert.equal(disclosure.phase, "closing");
});

test("without transitionend the fallback timer closes after the longest transition plus slack", async () => {
  // Longest: 0.2s + 0.05s delay = 250 ms, plus 50 ms of slack.
  await openPanel();
  await closePanel();

  await act(() => mock.timers.tick(299));
  assert.equal(disclosure.phase, "closing", "closed before the panel's own transition could end");

  await act(() => mock.timers.tick(1));
  assert.equal(disclosure.phase, "closed");
});

test("with transitions zeroed by the host the fallback still closes after the slack", async () => {
  panel.transitionDuration = "0s";
  panel.transitionDelay = "0s";
  await openPanel();
  await closePanel();

  await act(() => mock.timers.tick(50));

  assert.equal(disclosure.phase, "closed");
});

test("reopening during the close reverses straight to open, with no second enter", async () => {
  await openPanel();
  await closePanel();
  phases = [];

  await openPanel();

  assert.deepEqual(phases, ["open"]);
  assert.equal(panel.layoutReads, 1, "the reverse must not force another layout read");
});

test("after reopening mid-close the stale fallback timer does not close the panel", async () => {
  await openPanel();
  await closePanel();
  await openPanel();

  await act(() => mock.timers.tick(1000));
  await act(() => transitionEnd(panel, "opacity"));

  assert.equal(disclosure.phase, "open");
});

test("opening an open panel changes nothing", async () => {
  await openPanel();
  phases = [];

  await openPanel();

  assert.deepEqual(phases, []);
  assert.equal(panel.layoutReads, 1);
});

test("closing a closed panel changes nothing", async () => {
  await closePanel();

  assert.equal(disclosure.phase, "closed");
});

test("closeNow is in closing before it returns, ahead of the event's default action", async () => {
  await openPanel();
  let seen: DisclosurePhase | undefined;

  await act(() => {
    disclosure.closeNow();
    seen = disclosure.phase;
  });

  assert.equal(seen, "closing");
});
