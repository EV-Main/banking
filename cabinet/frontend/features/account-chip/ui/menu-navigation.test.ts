// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// A click on a link in the chip's open menu is offered to the host as one `ev:navigate` from
// the chip element; the click is cancelled only if the host took it, and the menu closes
// either way. Sign out is a button and offers nothing; a modified click (new tab) or a link
// that opens elsewhere stays the browser's and is offered nothing either. What decides "plain
// and same-origin" is pinned in lib/navigate-event.test.ts; this pins the panel's wiring.
//
// Rendered with react-dom's client renderer on the stand-in document of `__tests__/fake-dom.ts`
// (as in account-menu.test.ts), inside an element named like the chip's, so the event leaves
// from where a host listens. The page is `PAGE_URL` (https://cabinet.example/en/cabinet).
import assert from "node:assert/strict";
import test, { after, afterEach, before, beforeEach } from "node:test";

import { bundle } from "../__tests__/bundle.ts";
import { FakeDocument, FakeElement, click, descendants, installDom, type Modifiers } from "../__tests__/fake-dom.ts";

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

const PROPS = {
  identity: { label: "Valentina K.", email: "v@example.com", initials: "VK" },
  links: { profile: { href: "/en/cabinet", kind: "cabinet" as const }, manage: "/en/cabinet/settings?tab=security#top", switchAccount: "/api/auth/login" },
  groups: [
    {
      id: "services",
      label: "Services",
      items: [{ id: "docs", href: "https://cabinet.example/docs", label: "Docs", external: true, current: false }],
    },
  ],
  onSignOut: () => undefined,
};

/** The chip element; React renders into it as entry.tsx does. */
let element: FakeElement;
let unmount: () => Promise<void>;
/** Every `ev:navigate` the element dispatched, and whether the host stand-in takes them. */
let offers: CustomEvent[];
let hostTakes: boolean;

beforeEach(async () => {
  element = doc.body.appendChild(doc.createElement("mfe-cabinet-account-chip"));
  offers = [];
  hostTakes = false;
  element.addEventListener("ev:navigate", (e) => {
    assert.ok(e instanceof CustomEvent);
    offers.push(e);
    if (hostTakes) e.preventDefault();
  });
  const root = chip.createRoot(element as unknown as Element);
  unmount = async () => {
    await chip.act(() => root.unmount());
  };
  await chip.act(() => root.render(chip.createElement(chip.AccountMenu, PROPS)));
  await chip.act(() => void click(element, trigger(), 1));
  assert.equal(trigger().getAttribute("aria-expanded"), "true", "the menu did not open");
});

afterEach(async () => {
  await unmount();
  doc.body.removeChild(element);
});

function trigger(): FakeElement {
  const button = descendants(element).find((el) => el.tagName === "BUTTON");
  assert.ok(button, "the chip rendered no button");
  return button;
}

/** A row of the open panel by its visible text. */
function row(text: string): FakeElement {
  const panel = descendants(element).find((el) => el.id === trigger().getAttribute("aria-controls"));
  const found = panel && descendants(panel).find((el) => (el.tagName === "A" || el.tagName === "BUTTON") && el.textContent.includes(text));
  assert.ok(found, `no "${text}" row in the open panel`);
  return found;
}

async function press(target: FakeElement, modifiers: Modifiers = {}): Promise<Event> {
  return chip.act(() => click(element, target, 1, modifiers));
}

test("a plain click on a menu link offers its path to the host once", async () => {
  await press(row("Manage account"));

  assert.deepEqual(
    offers.map((e) => e.detail),
    [{ href: "/en/cabinet/settings?tab=security#top" }],
  );
});

test("when the host takes the path the click is cancelled and the menu closes", async () => {
  hostTakes = true;

  const clicked = await press(row("Manage account"));

  assert.equal(clicked.defaultPrevented, true, "the browser would still follow the link");
  assert.equal(trigger().getAttribute("aria-expanded"), "false");
});

test("when the host leaves the path the browser follows the link and the menu still closes", async () => {
  const clicked = await press(row("Manage account"));

  assert.equal(offers.length, 1);
  assert.equal(clicked.defaultPrevented, false, "the click was cancelled with no host to take it");
  assert.equal(trigger().getAttribute("aria-expanded"), "false");
});

test("a click on the name inside the profile row offers the row's link", async () => {
  const name = descendants(row("Valentina K.")).find((el) => el.tagName === "SPAN" && el.textContent === "Valentina K.");
  assert.ok(name, "no name span in the profile row");

  await press(name);

  assert.deepEqual(
    offers.map((e) => e.detail),
    [{ href: "/en/cabinet" }],
  );
});

test("Sign out offers nothing", async () => {
  await press(row("Sign out"));

  assert.equal(offers.length, 0);
});

test("a ctrl-click offers nothing and is left to the browser", async () => {
  hostTakes = true;

  const clicked = await press(row("Manage account"), { ctrlKey: true });

  assert.equal(offers.length, 0);
  assert.equal(clicked.defaultPrevented, false);
});

test("a link that opens in a new tab offers nothing", async () => {
  hostTakes = true;

  const clicked = await press(row("Docs"));

  assert.equal(offers.length, 0);
  assert.equal(clicked.defaultPrevented, false);
});
