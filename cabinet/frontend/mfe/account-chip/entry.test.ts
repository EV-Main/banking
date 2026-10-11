// Run with `node --test --experimental-strip-types mfe/account-chip/entry.test.ts`.
//
// `<mfe-cabinet-account-chip>` tells its host when it has painted: `data-ready` appears once
// the stylesheet has applied and the chip rendered, in the same step that drops the invisible
// size reserve, and goes away on disconnect. Hosts wait on `[data-ready]` instead of watching
// the inline style. What is pinned here: not ready (and reserved) before the sheet; ready,
// painted and unreserved after it; not ready after disconnect; and a sheet that lands for a
// connection that is already gone marks nothing ready.
//
// The element runs in host mode (`data-mode="host"`), which fetches nothing. Its stylesheet
// loader is replaced by a stub whose promises the tests settle; custom element reactions do
// not exist in the stand-in DOM, so the tests call the lifecycle callbacks the browser would.
import assert from "node:assert/strict";
import test, { after, afterEach, before } from "node:test";

import { bundle } from "../../features/account-chip/__tests__/bundle.ts";
import { FakeDocument, FakeElement, installDom } from "../../features/account-chip/__tests__/fake-dom.ts";

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
});

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
