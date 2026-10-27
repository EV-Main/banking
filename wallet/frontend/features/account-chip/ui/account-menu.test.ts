// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// `AccountMenu` with `locked` is the chip painted from the last visit's cache before the
// session confirmed it — on a shared machine, possibly the previous person. What is pinned
// here: a locked chip has no panel in the DOM at all, never shows the email, is announced as
// disabled and busy while staying in the tab order, and no click or arrow key opens it; and
// a sign-out error is shown under the closed chip and steps aside while the panel is open.
// The unlocked cases are the control: the same gestures do open it in this harness.
//
// react-dom's client renderer runs against the stand-in document in `shared/__tests__/fake-dom.ts`
// (the same approach as use-disclosure.test.ts, with the few node methods a rendered tree
// needs); the component is compiled with esbuild because Node cannot load TSX.
import assert from "node:assert/strict";
import test, { after, afterEach, before, beforeEach } from "node:test";

import { bundle } from "../../../shared/__tests__/bundle.ts";
import { FakeDocument, FakeElement, click, descendants, installDom, keyDown } from "../../../shared/__tests__/fake-dom.ts";
import type { AccountMenuProps } from "./account-menu.tsx";

type Loaded = Pick<typeof import("react"), "act" | "createElement"> &
  Pick<typeof import("react-dom/client"), "createRoot"> &
  Pick<typeof import("./account-menu.tsx"), "AccountMenu">;

const doc = new FakeDocument();
let uninstall: () => void;
let chip: Loaded;

before(async () => {
  uninstall = installDom(doc);
  // The bundle is built from exactly these exports; the cast names them for the compiler.
  chip = (await bundle(`export { AccountMenu } from "@/features/account-chip/ui/account-menu";`)) as Loaded;
});

after(() => uninstall());

const EMAIL = "v@example.com";
const IDENTITY = { label: "Valentina K.", email: EMAIL, initials: "VK" };
const LINKS = { profile: { href: "/en/cabinet", kind: "cabinet" as const }, manage: "/en/cabinet/settings", switchAccount: null };
const SIGN_OUT_ERROR = "We couldn't sign you out. Please try again.";

let container: FakeElement;
let unmount: () => Promise<void>;

beforeEach(() => {
  container = doc.body.appendChild(doc.createElement("div"));
  doc.activeElement = null;
});

afterEach(async () => {
  await unmount();
  doc.body.removeChild(container);
});

async function render(props: Partial<AccountMenuProps>): Promise<void> {
  const root = chip.createRoot(container as unknown as Element);
  unmount = async () => {
    await chip.act(() => root.unmount());
  };
  const all: AccountMenuProps = { identity: IDENTITY, links: LINKS, groups: [], onSignOut: () => undefined, ...props };
  await chip.act(() => root.render(chip.createElement(chip.AccountMenu, all)));
}

/** The chip's own button: the first one in the tree. */
function trigger(): FakeElement {
  const button = descendants(container).find((el) => el.tagName === "BUTTON");
  assert.ok(button, "the chip rendered no button");
  return button;
}

/** The element the trigger says it controls, or undefined when it is not in the DOM. */
function panel(): FakeElement | undefined {
  const id = trigger().getAttribute("aria-controls");
  return descendants(container).find((el) => el.id === id);
}

function alert(): FakeElement | undefined {
  return descendants(container).find((el) => el.getAttribute("role") === "alert");
}

// ── locked ──────────────────────────────────────────────────────────────────

test("a locked chip has no panel in the DOM", async () => {
  await render({ locked: true });

  assert.equal(panel()?.outerHTML, undefined);
});

test("a locked chip is announced as disabled, busy and collapsed, yet stays focusable", async () => {
  await render({ locked: true });

  assert.equal(trigger().getAttribute("aria-disabled"), "true");
  assert.equal(trigger().getAttribute("aria-busy"), "true");
  assert.equal(trigger().getAttribute("aria-expanded"), "false");
  assert.equal(trigger().hasAttribute("disabled"), false, "native disabled would drop the chip from the tab order");
});

test("a locked chip's markup never carries the email", async () => {
  await render({ locked: true });

  assert.equal(container.outerHTML.includes(EMAIL), false, container.outerHTML);
});

test("clicking a locked chip opens nothing, by pointer or by Enter", async () => {
  await render({ locked: true });

  await chip.act(() => void click(container, trigger(), 1));
  await chip.act(() => void click(container, trigger(), 0));

  assert.equal(panel()?.outerHTML, undefined);
  assert.equal(trigger().getAttribute("aria-expanded"), "false");
});

test("ArrowDown and ArrowUp on a locked chip open nothing and leave the key to the page", async () => {
  await render({ locked: true });

  const down = await chip.act(() => keyDown(container, trigger(), "ArrowDown"));
  const up = await chip.act(() => keyDown(container, trigger(), "ArrowUp"));

  assert.equal(panel()?.outerHTML, undefined);
  assert.equal(trigger().getAttribute("aria-expanded"), "false");
  assert.equal(down.defaultPrevented, false, "ArrowDown was swallowed");
  assert.equal(up.defaultPrevented, false, "ArrowUp was swallowed");
});

// ── unlocked: the control ───────────────────────────────────────────────────

test("an unlocked chip carries no lock state and opens its panel, email included, on click", async () => {
  await render({});
  assert.equal(trigger().hasAttribute("aria-disabled"), false);
  assert.equal(trigger().hasAttribute("aria-busy"), false);

  await chip.act(() => void click(container, trigger(), 1));

  assert.equal(trigger().getAttribute("aria-expanded"), "true");
  assert.equal(panel()?.getAttribute("data-state"), "open");
  assert.equal(panel()?.textContent.includes(EMAIL), true);
});

test("ArrowDown on an unlocked chip opens the panel and focuses its first entry", async () => {
  await render({});

  const down = await chip.act(() => keyDown(container, trigger(), "ArrowDown"));

  assert.equal(trigger().getAttribute("aria-expanded"), "true");
  assert.equal(down.defaultPrevented, true);
  assert.equal(doc.activeElement?.getAttribute("href"), "/en/cabinet");
});

// ── error ───────────────────────────────────────────────────────────────────

test("a sign-out error is announced under the closed chip", async () => {
  await render({ error: SIGN_OUT_ERROR });

  assert.equal(alert()?.textContent, SIGN_OUT_ERROR);
});

test("the error steps aside while the panel is open and returns once it closes", async () => {
  await render({ error: SIGN_OUT_ERROR });

  await chip.act(() => void click(container, trigger(), 1));
  assert.equal(trigger().getAttribute("aria-expanded"), "true");
  assert.equal(alert()?.outerHTML, undefined, "the alert overlaps the open panel");

  await chip.act(() => void doc.dispatchEvent(Object.assign(new Event("keydown"), { key: "Escape" })));
  assert.equal(trigger().getAttribute("aria-expanded"), "false");
  assert.equal(alert()?.textContent, SIGN_OUT_ERROR);
});

test("no error, no alert", async () => {
  await render({ error: null });

  assert.equal(alert()?.outerHTML, undefined);
});
