// Run with `node --test --experimental-strip-types mfe/account-chip/stylesheet.test.ts`.
//
// `whenStylesheetReady` gates the chip's first paint, on every page of every host. What is
// pinned here: a sheet already in the head is reused, never fetched again; a successful
// fetch lands as exactly one `<style>` that already holds its CSS when it is attached (an
// empty one filled afterwards re-triggers the host's font-set rebuild the inline sheet
// exists to avoid); a failed fetch falls back to a `<link>`; and nothing — a hung fetch, a
// lost load event — keeps the chip hidden past four seconds.
//
// No DOM library is installed, so a stand-in document carries exactly what the module
// touches: `head` with `querySelectorAll("link, style")`, `createElement`, `styleSheets`
// and capture-phase `load`/`error` listeners. The module memoises its promise per page, so
// every test imports a fresh copy.
import assert from "node:assert/strict";
import test, { afterEach, beforeEach, mock } from "node:test";

const TAG = "mfe-ev-account-chip";
const HREF = "https://cabinet.example/cabinet/mfe/account-chip.css";
const CSS = ".chip{color:red}";

class FakeElement extends EventTarget {
  dataset: Record<string, string> = {};
  textContent: string | null = null;
}
class FakeLink extends FakeElement {
  rel = "";
  href = "";
}
class FakeStyle extends FakeElement {}
class FakeImportRule {}
class FakeLayerStatementRule {}

type Appended = { node: FakeElement; textAtAppend: string | null };

class FakeDocument extends EventTarget {
  baseURI = "https://host.example/";
  styleSheets: { href: string | null; cssRules: unknown[] }[] = [];
  appended: Appended[] = [];
  #head: FakeElement[] = [];
  head = {
    // The module only ever asks for "link, style", and the head holds nothing else.
    querySelectorAll: () => this.#head.slice(),
    appendChild: (node: FakeElement) => {
      this.appended.push({ node, textAtAppend: node.textContent });
      this.#head.push(node);
      return node;
    },
  };
  createElement(name: string): FakeElement {
    if (name === "link") return new FakeLink();
    if (name === "style") return new FakeStyle();
    throw new Error(`unexpected createElement(${name})`);
  }
  /** Put a node in the head as if the server had rendered it — not counted as appended. */
  preload(node: FakeElement): void {
    this.#head.push(node);
  }
  /** Fire `type` with `target` as its target, the way an element's load/error reaches capture listeners. */
  fire(type: "load" | "error", target: FakeElement): void {
    const event = new Event(type);
    Object.defineProperty(event, "target", { value: target });
    this.dispatchEvent(event);
  }
}

let doc: FakeDocument;
let fetchCalls: string[];

beforeEach(() => {
  doc = new FakeDocument();
  fetchCalls = [];
  // Mocked in every test: a case left pending must not keep a real four-second timer alive.
  mock.timers.enable({ apis: ["setTimeout"] });
  Reflect.set(globalThis, "document", doc);
  Reflect.set(globalThis, "HTMLLinkElement", FakeLink);
  Reflect.set(globalThis, "HTMLStyleElement", FakeStyle);
  Reflect.set(globalThis, "CSSImportRule", FakeImportRule);
  Reflect.set(globalThis, "CSSLayerStatementRule", FakeLayerStatementRule);
});

afterEach(() => {
  mock.restoreAll();
  mock.timers.reset();
  for (const name of ["document", "HTMLLinkElement", "HTMLStyleElement", "CSSImportRule", "CSSLayerStatementRule"]) {
    Reflect.deleteProperty(globalThis, name);
  }
});

/** `fetch` answered by `respond`, with every requested URL recorded. */
function stubFetch(respond: () => Promise<Response>): void {
  mock.method(globalThis, "fetch", (input: string) => {
    fetchCalls.push(String(input));
    return respond();
  });
}

let instance = 0;
const freshModule = (): Promise<typeof import("./stylesheet.ts")> => import(`./stylesheet.ts?instance=${++instance}`);

/** Lets every queued promise callback run; real `setImmediate`, untouched by the mocked timers. */
const settle = () => new Promise<void>((resolve) => setImmediate(resolve));

/** "resolved" if `promise` has settled by now, else "pending". */
async function state(promise: Promise<unknown>): Promise<"resolved" | "pending"> {
  await settle();
  return Promise.race([promise.then(() => "resolved" as const), settle().then(() => "pending" as const)]);
}

const link = (href: string, tag?: string): FakeLink => {
  const node = new FakeLink();
  node.rel = "stylesheet";
  node.href = href;
  if (tag) node.dataset.mfe = tag;
  return node;
};

// ── already in the head ─────────────────────────────────────────────────────

test("an inline sheet already in the head is ready at once and nothing is fetched", async () => {
  const style = new FakeStyle();
  style.dataset.mfe = TAG;
  doc.preload(style);
  stubFetch(() => Promise.resolve(new Response(CSS)));
  const { whenStylesheetReady } = await freshModule();

  assert.equal(await state(whenStylesheetReady(TAG, HREF)), "resolved");
  assert.deepEqual(fetchCalls, []);
  assert.equal(doc.appended.length, 0);
});

test("a linked sheet already in the head is waited on, not fetched", async () => {
  const existing = link(HREF, TAG);
  doc.preload(existing);
  stubFetch(() => Promise.resolve(new Response(CSS)));
  const { whenStylesheetReady } = await freshModule();

  const ready = whenStylesheetReady(TAG, HREF);
  assert.equal(await state(ready), "pending", "the link has not loaded yet");

  doc.styleSheets.push({ href: HREF, cssRules: [{}] });
  doc.fire("load", existing);

  assert.equal(await state(ready), "resolved");
  assert.deepEqual(fetchCalls, []);
  assert.equal(doc.appended.length, 0);
});

// ── fetched and inlined ─────────────────────────────────────────────────────

test("a fetched sheet is attached as one filled style in the reamfe layer", async () => {
  stubFetch(() => Promise.resolve(new Response(CSS, { status: 200 })));
  const { whenStylesheetReady } = await freshModule();

  assert.equal(await state(whenStylesheetReady(TAG, HREF)), "resolved");

  assert.deepEqual(fetchCalls, [HREF]);
  assert.equal(doc.appended.length, 1);
  const [only] = doc.appended;
  assert.ok(only?.node instanceof FakeStyle, "expected a <style>, got a <link>");
  assert.equal(only.node.dataset.mfe, TAG);
  assert.equal(only.textAtAppend, `@layer reamfe{${CSS}}`, "the style must hold its CSS before it is attached");
});

test("two calls on one page share one fetch and one style", async () => {
  stubFetch(() => Promise.resolve(new Response(CSS)));
  const { whenStylesheetReady } = await freshModule();

  const first = whenStylesheetReady(TAG, HREF);
  const second = whenStylesheetReady(TAG, HREF);

  assert.equal(first, second);
  assert.equal(await state(second), "resolved");
  assert.deepEqual(fetchCalls, [HREF]);
  assert.equal(doc.appended.length, 1);
});

test("a style another copy attached during the fetch is not doubled", async () => {
  let answer!: (res: Response) => void;
  stubFetch(() => new Promise<Response>((resolve) => (answer = resolve)));
  const { whenStylesheetReady } = await freshModule();

  const ready = whenStylesheetReady(TAG, HREF);
  const rival = new FakeStyle();
  rival.dataset.mfe = TAG;
  doc.preload(rival);
  answer(new Response(CSS));

  assert.equal(await state(ready), "resolved");
  assert.equal(doc.appended.length, 0);
});

// ── fallback to a link ──────────────────────────────────────────────────────

test("an HTTP error falls back to a link that is waited on", async () => {
  stubFetch(() => Promise.resolve(new Response("unavailable", { status: 503 })));
  const { whenStylesheetReady } = await freshModule();

  const ready = whenStylesheetReady(TAG, HREF);
  assert.equal(await state(ready), "pending", "the fallback link has not loaded yet");

  assert.equal(doc.appended.length, 1);
  const [only] = doc.appended;
  assert.ok(only?.node instanceof FakeLink, "expected a fallback <link>");
  assert.equal(only.node.href, HREF);
  assert.equal(only.node.rel, "stylesheet");
  assert.equal(only.node.dataset.mfe, TAG);

  doc.styleSheets.push({ href: HREF, cssRules: [{}] });
  doc.fire("load", only.node);
  assert.equal(await state(ready), "resolved");
});

test("a network error falls back to a link, and its load error releases the chip", async () => {
  stubFetch(() => Promise.reject(new TypeError("Failed to fetch")));
  const { whenStylesheetReady } = await freshModule();

  const ready = whenStylesheetReady(TAG, HREF);
  assert.equal(await state(ready), "pending");

  const [only] = doc.appended;
  assert.ok(only?.node instanceof FakeLink, "expected a fallback <link>");
  doc.fire("error", only.node);
  assert.equal(await state(ready), "resolved");
});

test("an error from some other element does not release the chip", async () => {
  stubFetch(() => Promise.reject(new TypeError("Failed to fetch")));
  const { whenStylesheetReady } = await freshModule();

  const ready = whenStylesheetReady(TAG, HREF);
  await settle();
  doc.fire("error", link("https://host.example/other.css"));

  assert.equal(await state(ready), "pending");
});

// ── the cap ─────────────────────────────────────────────────────────────────

test("a fetch that never answers releases the chip after four seconds", async () => {
  stubFetch(() => new Promise<Response>(() => undefined));
  const { whenStylesheetReady } = await freshModule();

  const ready = whenStylesheetReady(TAG, HREF);
  mock.timers.tick(3999);
  assert.equal(await state(ready), "pending", "released before the cap");

  mock.timers.tick(1);
  assert.equal(await state(ready), "resolved");
});

test("a link whose load event is lost releases the chip after four seconds", async () => {
  doc.preload(link(HREF, TAG));
  stubFetch(() => Promise.resolve(new Response(CSS)));
  const { whenStylesheetReady } = await freshModule();

  const ready = whenStylesheetReady(TAG, HREF);
  mock.timers.tick(3999);
  assert.equal(await state(ready), "pending", "released before the cap");

  mock.timers.tick(1);
  assert.equal(await state(ready), "resolved");
});
