// Run with `node --test --experimental-strip-types mfe/account-chip/entry.test.ts`.
//
// `<mfe-cabinet-account-chip>` tells its host when it has painted: `data-ready` appears once
// the stylesheet has applied and the chip rendered, in the same step that drops the invisible
// size reserve, and goes away on disconnect. Hosts wait on `[data-ready]` instead of watching
// the inline style. What is pinned here: not ready (and reserved) before the sheet; ready,
// painted and unreserved after it; not ready after disconnect; and a sheet that lands for a
// connection that is already gone marks nothing ready.
//
// Until then the chip is invisible, and its box is the host's when the host gave the element
// one (a class with `display: flex`, or `hidden` while signed out): then only `visibility` is
// held inline, since an inline display or size would beat the host's. Only an element left in
// inline flow, or with no computed display at all, gets the chip's own 178 × 42 reserve.
// Disconnecting clears every reserve key, so the next connect reads the host's display again
// and not a leftover of ours.
//
// The element runs in host mode (`data-mode="host"`), which fetches nothing. Its stylesheet
// loader is replaced by a stub whose promises the tests settle; custom element reactions do
// not exist in the stand-in DOM, so the tests call the lifecycle callbacks the browser would.
// `getComputedStyle` answers `display` as a browser does: the element's inline display if it
// has one, else what the host's stylesheet gives it (`hostDisplay`, set per test).
import assert from "node:assert/strict";
import test, { after, afterEach, before } from "node:test";

import { bundle } from "../../shared/__tests__/bundle.ts";
import { FakeDocument, FakeElement, installDom } from "../../shared/__tests__/fake-dom.ts";

const TAG = "mfe-cabinet-account-chip";

// Every `whenStylesheetReady` call parks its resolver here, in call order.
const STYLESHEET_STUB = `
export const sheetRequests: Array<() => void> = [];
export const stylesheetHref = (): string => "/cabinet/mfe/account-chip.css";
export const whenStylesheetReady = (): Promise<void> => new Promise((resolve) => void sheetRequests.push(() => resolve()));
`;

type ChipElement = FakeElement & { connectedCallback(): void; disconnectedCallback(): void };
type Loaded = Pick<typeof import("react"), "act"> & { sheetRequests: Array<() => void> };

const doc = new FakeDocument();
/** The display the host's own CSS gives the element; a custom element defaults to inline. */
let hostDisplay = "inline";
const registry = new Map<string, new () => ChipElement>();
let uninstall: () => void;
let chip: Loaded;

before(async () => {
  uninstall = installDom(doc);
  Reflect.set(
    globalThis,
    "HTMLElement",
    class extends FakeElement {
      constructor() {
        super(doc, TAG);
      }
    },
  );
  Reflect.set(globalThis, "getComputedStyle", (el: FakeElement) => ({
    display: el.style.getPropertyValue("display") || hostDisplay,
    transitionDuration: "0s",
    transitionDelay: "0s",
  }));
  Reflect.set(globalThis, "customElements", {
    get: (name: string) => registry.get(name),
    define: (name: string, ctor: new () => ChipElement) => void registry.set(name, ctor),
  });
  // The bundle is built from exactly these exports; the cast names them for the compiler.
  chip = (await bundle(`import "@/mfe/account-chip/entry";\nexport { sheetRequests } from "./stylesheet";`, {
    "./stylesheet": STYLESHEET_STUB,
  })) as Loaded;
});

after(() => uninstall());

const connected: ChipElement[] = [];

afterEach(async () => {
  for (const el of connected.splice(0)) {
    await chip.act(() => el.disconnectedCallback());
    if (el.parentNode) doc.body.removeChild(el);
  }
  chip.sheetRequests.length = 0;
  hostDisplay = "inline";
});

/** The element's inline style as plain `property: value` pairs. */
function inlineStyle(el: FakeElement): Record<string, string> {
  return Object.fromEntries(Object.entries(el.style).filter((entry): entry is [string, string] => typeof entry[1] === "string"));
}

const RESERVE = { visibility: "hidden", display: "inline-block", width: "178px", height: "42px" };

/** A host-mode element, placed in the body and connected as the browser would. */
async function connect(el?: ChipElement): Promise<ChipElement> {
  const Element = registry.get(TAG);
  assert.ok(Element, `${TAG} was not defined`);
  const chipEl = el ?? new Element();
  if (!el) {
    chipEl.setAttribute("data-mode", "host");
    chipEl.setAttribute("data-name", "Valentina Kuznetsova");
    chipEl.setAttribute("data-email", "v@example.com");
  }
  doc.body.appendChild(chipEl);
  connected.push(chipEl);
  await chip.act(() => chipEl.connectedCallback());
  return chipEl;
}

async function disconnect(el: ChipElement): Promise<void> {
  doc.body.removeChild(el);
  connected.splice(connected.indexOf(el), 1);
  await chip.act(() => el.disconnectedCallback());
}

/** Settle the n-th stylesheet wait, and let the chip render behind it. */
async function stylesheetApplies(n: number): Promise<void> {
  const resolve = chip.sheetRequests[n];
  assert.ok(resolve, `no stylesheet request #${n}`);
  await chip.act(async () => resolve());
}

test("before its stylesheet applies the element holds an invisible reserve and is not ready", async () => {
  const el = await connect();

  assert.equal(el.hasAttribute("data-ready"), false);
  assert.equal(el.style.getPropertyValue("visibility"), "hidden");
  assert.equal(el.style.getPropertyValue("width"), "178px");
  assert.equal(el.style.getPropertyValue("height"), "42px");
  assert.equal(el.textContent, "");
});

test("once the stylesheet applies the chip paints, the reserve is released and data-ready is set", async () => {
  const el = await connect();

  await stylesheetApplies(0);

  assert.equal(el.getAttribute("data-ready"), "");
  assert.equal(el.style.getPropertyValue("visibility"), "");
  assert.equal(el.style.getPropertyValue("width"), "");
  assert.equal(el.textContent.includes("Valentina Kuznetsova"), true, el.outerHTML);
});

test("disconnecting removes data-ready along with the chip", async () => {
  const el = await connect();
  await stylesheetApplies(0);

  await disconnect(el);

  assert.equal(el.hasAttribute("data-ready"), false);
  assert.equal(el.textContent, "");
});

test("a stylesheet that lands after disconnect marks nothing ready", async () => {
  const el = await connect();
  await disconnect(el);

  await stylesheetApplies(0);

  assert.equal(el.hasAttribute("data-ready"), false);
  assert.equal(el.textContent, "");
});

test("a reconnected element waits for its own stylesheet, not the first connection's", async () => {
  const el = await connect();
  await disconnect(el);
  await connect(el);

  await stylesheetApplies(0);
  assert.equal(el.hasAttribute("data-ready"), false, "ready on the stale connection's stylesheet");

  await stylesheetApplies(1);
  assert.equal(el.getAttribute("data-ready"), "");
  assert.equal(el.textContent.includes("Valentina Kuznetsova"), true, el.outerHTML);
});

// ── the reserve against the host's own box ──────────────────────────────────

test("a host that makes the element flex keeps its box: only visibility is held", async () => {
  hostDisplay = "flex";

  const el = await connect();

  assert.deepEqual(inlineStyle(el), { visibility: "hidden" });
});

test("a host that hides the element keeps it hidden: only visibility is held", async () => {
  hostDisplay = "none";

  const el = await connect();

  assert.deepEqual(inlineStyle(el), { visibility: "hidden" });
});

test("an element left in inline flow gets the chip's own 178 × 42 reserve", async () => {
  hostDisplay = "inline";

  const el = await connect();

  assert.deepEqual(inlineStyle(el), RESERVE);
});

test("an element with no computed display gets the reserve", async () => {
  hostDisplay = "";

  const el = await connect();

  assert.deepEqual(inlineStyle(el), RESERVE);
});

test("on a host box the stylesheet's arrival leaves no inline style behind", async () => {
  hostDisplay = "flex";
  const el = await connect();

  await stylesheetApplies(0);

  assert.deepEqual(inlineStyle(el), {});
  assert.equal(el.getAttribute("data-ready"), "");
});

test("disconnecting while the reserve is held clears every key of it", async () => {
  const el = await connect();

  await disconnect(el);

  assert.deepEqual(inlineStyle(el), {});
});

test("a reconnect reads the host's display again, not a reserve left from before", async () => {
  const el = await connect();
  await disconnect(el);
  hostDisplay = "flex";

  await connect(el);

  assert.deepEqual(inlineStyle(el), { visibility: "hidden" });
});

test("a reconnect into inline flow takes the reserve after a host box", async () => {
  hostDisplay = "flex";
  const el = await connect();
  await disconnect(el);
  hostDisplay = "inline";

  await connect(el);

  assert.deepEqual(inlineStyle(el), RESERVE);
});
