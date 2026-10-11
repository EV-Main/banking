// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The chip's CSS sits in the `reamfe` layer, below every layer of the page that hosts it, so
// any host rule that matches a chip element wins on what it sets (chip-parts.ts). Two rules
// keep the chip's looks on a host, and both are pinned here:
//
//   1. A look switched by state never takes its base from a stock utility a host may ship in
//      its own `utilities` (`opacity-0`, `hidden`, `bg-transparent`): the closed look is the
//      variant (`data-[state=closed]:opacity-0`), the base is the default or an arbitrary
//      property. Checked by a SOURCE SCAN of every double-quoted string in ui/: a template
//      literal or a class assembled by concatenation is not seen.
//
//   2. A class on a property the host's preflight resets on that element is important (`px-2!`):
//      on every element `*` zeroes padding, margin and border; <button> also gets font, color,
//      border-radius and background reset, <a> color and text-decoration. A row is an <a> or
//      the Sign out <button> with the same ENTRY classes, so <a> is held to the button's list
//      (font size, radius, background too). Once a property's base is important, every state variant of it must be
//      too, or the base outranks the hover/active/aria/disabled look. Checked on the RENDERED
//      class attributes of the trigger, the panel (PANEL), its rows and Sign out (ENTRY with
//      CHIP_PRESS), its separators (RULE) and the error alert — the strings as they ship,
//      after `cn` merged them, whatever literal or concatenation produced them.
//      The limits: only what renders in the states drawn here (open panel with every link,
//      a labelled group with a current and an external item; the alert while closed), and
//      only the utilities `property()` below knows; an unknown class is not judged.
//
// None of this shows without a host page whose rules outrank the chip, which no unit test
// here has; these tests keep the convention from eroding one class at a time.
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import test, { after, before } from "node:test";

import { bundle } from "../__tests__/bundle.ts";
import { FakeDocument, FakeElement, click, descendants, installDom } from "../__tests__/fake-dom.ts";

// ── 1. no stock utility as a state base (source scan) ───────────────────────

const UI = new URL("./", import.meta.url);

/** Stock utilities a host is likely to ship that set a property the chip varies by state. */
const STATE_UTILITIES = new Set(["opacity-0", "scale-97", "-translate-y-1", "hidden", "bg-transparent"]);

const SOURCES = readdirSync(UI).filter((f) => /\.tsx?$/.test(f) && !f.endsWith(".test.ts"));

/** `file: "class"` for every bare state utility in a double-quoted string of the file. */
function bareStateUtilities(file: string): string[] {
  const source = readFileSync(new URL(file, UI), "utf8");
  return [...source.matchAll(/"([^"\n]*)"/g)].flatMap(([, literal = ""]) =>
    literal
      .split(/\s+/)
      // Important or not, a stock base is still the host's to outrank or to share.
      .filter((token) => STATE_UTILITIES.has(token.replace(/^!|!$/g, "")))
      .map((token) => `${file}: "${token}" in "${literal}"`),
  );
}

test("the scan sees the chip's ui sources", () => {
  assert.ok(SOURCES.includes("menu-panel.tsx"), `ui/ has: ${SOURCES.join(", ")}`);
  assert.ok(SOURCES.includes("chip-parts.ts"), `ui/ has: ${SOURCES.join(", ")}`);
});

test("no chip state takes its base look from a stock utility a host may outrank", () => {
  const found = SOURCES.flatMap(bareStateUtilities);

  assert.deepEqual(
    found,
    [],
    "a bare state utility loses to the host's own utilities layer; make the state the variant " +
      "(data-[state=…]:, aria-*:, motion-safe:) and leave the base to the default or an arbitrary property",
  );
});

// ── 2. important where the host's preflight resets (rendered classes) ───────

type Loaded = Pick<typeof import("react"), "act" | "createElement"> &
  Pick<typeof import("react-dom/client"), "createRoot"> &
  Pick<typeof import("./account-menu.tsx"), "AccountMenu">;

/** A class split into its variants and its utility; `important` is the `!` either side. */
type Token = { cls: string; variants: string[]; utility: string; important: boolean };

/** Split on the variant colons, not the ones inside `[...]` or `(...)`. */
function token(cls: string): Token {
  const parts: string[] = [];
  let depth = 0;
  let current = "";
  for (const ch of cls) {
    if (ch === "[" || ch === "(") depth += 1;
    if (ch === "]" || ch === ")") depth -= 1;
    if (ch === ":" && depth === 0) {
      parts.push(current);
      current = "";
    } else current += ch;
  }
  const important = current.startsWith("!") || current.endsWith("!");
  return { cls, variants: parts, utility: current.replace(/^!|!$/g, ""), important };
}

/** The CSS property a utility sets, for the properties a preflight reset touches; else null. */
function property(utility: string): string | null {
  const arbitrary = /^\[([a-z-]+):/.exec(utility);
  if (arbitrary) return arbitrary[1] === "border-color" ? "border" : (arbitrary[1] ?? null);
  if (/^-?p[xytrbl]?-/.test(utility)) return "padding";
  if (/^-?m[xytrbl]?-/.test(utility)) return "margin";
  if (/^border(-|$)/.test(utility)) return "border";
  if (/^rounded(-|$)/.test(utility)) return "border-radius";
  if (/^bg-/.test(utility)) return "background-color";
  if (/^text-(xs|sm|base|lg|[2-9]?xl)$/.test(utility)) return "font-size";
  if (/^text-(left|right|center|justify|start|end)$/.test(utility)) return "text-align";
  if (/^text-/.test(utility)) return "color";
  if (/^(no-)?underline$/.test(utility)) return "text-decoration";
  if (/^cursor-/.test(utility)) return "cursor";
  return null;
}

/** What Tailwind's preflight resets, by element. */
const RESET_EVERYWHERE = ["padding", "margin", "border"];
const RESET_ON: Record<string, string[]> = {
  A: [...RESET_EVERYWHERE, "color", "text-decoration", "font-size", "border-radius", "background-color"],
  BUTTON: [...RESET_EVERYWHERE, "color", "font-size", "border-radius", "background-color"],
};

/** A checked element: where its classes come from, for the failure message. */
type Checked = { name: string; tag: string; classes: string[] };

const doc = new FakeDocument();
let uninstall: () => void;
let checked: Checked[];

before(async () => {
  uninstall = installDom(doc);
  // The bundle is built from exactly these exports; the cast names them for the compiler.
  const chip = (await bundle(`export { AccountMenu } from "@/features/account-chip/ui/account-menu";`)) as Loaded;
  const container = doc.body.appendChild(doc.createElement("div"));
  const root = chip.createRoot(container as unknown as Element);
  const props = {
    identity: { label: "Valentina K.", email: "v@example.com", initials: "VK" },
    links: { profile: { href: "/en/cabinet", kind: "cabinet" as const }, manage: "/en/cabinet/settings", switchAccount: "/api/auth/login" },
    groups: [
      {
        id: "services",
        label: "Services",
        items: [
          { id: "sa", href: "https://sa.example/", label: "Service-Arb", external: false, current: true },
          { id: "ext", href: "https://docs.example/", label: "Docs", external: true, current: false },
        ],
      },
    ],
    error: "We couldn't sign you out. Please try again.",
    onSignOut: () => undefined,
  };
  await chip.act(() => root.render(chip.createElement(chip.AccountMenu, props)));
  const of = (el: FakeElement, name: string): Checked => ({ name, tag: el.tagName, classes: (el.getAttribute("class") ?? "").split(/\s+/).filter(Boolean) });
  const all = () => descendants(container);
  const alert = all().find((el) => el.getAttribute("role") === "alert");
  const trigger = all().find((el) => el.tagName === "BUTTON");
  assert.ok(alert && trigger, "the closed chip rendered no trigger or no alert");
  const closed = [of(trigger, "trigger + CHIP_PRESS (account-menu.tsx, chip-parts.ts)"), of(alert, "error alert (account-menu.tsx)")];

  await chip.act(() => void click(container, trigger, 1));
  const panel = all().find((el) => el.id === trigger.getAttribute("aria-controls"));
  assert.ok(panel, "the chip did not open");
  const inPanel = descendants(panel);
  checked = [
    ...closed,
    of(panel, "PANEL (menu-panel.tsx)"),
    ...inPanel.filter((el) => el.tagName === "A" || el.tagName === "BUTTON").map((el) => of(el, `ENTRY + CHIP_PRESS on <${el.tagName.toLowerCase()}> "${el.textContent}" (menu-panel.tsx, chip-parts.ts)`)),
    ...inPanel.filter((el) => el.tagName === "HR").map((el) => of(el, "RULE (menu-panel.tsx)")),
  ];
  await chip.act(() => root.unmount());
});

after(() => uninstall());

test("the check sees the trigger, the alert, the panel, every row and every rule", () => {
  const names = checked.map((c) => c.name.replace(/ ".*"/, ""));

  assert.deepEqual(names, [
    "trigger + CHIP_PRESS (account-menu.tsx, chip-parts.ts)",
    "error alert (account-menu.tsx)",
    "PANEL (menu-panel.tsx)",
    // profile row, manage, two service items, switch account, sign out
    ...Array<string>(5).fill("ENTRY + CHIP_PRESS on <a> (menu-panel.tsx, chip-parts.ts)"),
    "ENTRY + CHIP_PRESS on <button> (menu-panel.tsx, chip-parts.ts)",
    "RULE (menu-panel.tsx)",
    "RULE (menu-panel.tsx)",
  ]);
});

test("every chip class on a property the host's preflight resets on that element is important", () => {
  const found = checked.flatMap(({ name, tag, classes }) =>
    classes
      .map(token)
      .filter((t) => t.variants.length === 0 && !t.important)
      .flatMap((t) => {
        const prop = property(t.utility);
        return prop && (RESET_ON[tag] ?? RESET_EVERYWHERE).includes(prop)
          ? [`${name}: "${t.cls}" — host preflight resets ${prop} on <${tag.toLowerCase()}>; write "${t.cls}!"`]
          : [];
      }),
  );

  assert.deepEqual(found, [], "a plain class here loses to the host's reset in any later layer");
});

test("every state variant of a property with an important base is important too", () => {
  const found = checked.flatMap(({ name, classes }) => {
    const tokens = classes.map(token);
    const importantBase = new Set(tokens.filter((t) => t.variants.length === 0 && t.important).map((t) => property(t.utility)));
    importantBase.delete(null);
    return tokens
      .filter((t) => t.variants.length > 0 && !t.important && importantBase.has(property(t.utility)))
      .map((t) => `${name}: "${t.cls}" — the important base of ${property(t.utility)} outranks this state; write "${t.cls}!"`);
  });

  assert.deepEqual(found, [], "a plain state variant never shows over an important base");
});
