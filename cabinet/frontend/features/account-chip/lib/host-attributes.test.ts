// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Host mode reads everything it renders off the element's attributes, set by a page on
// another origin. What is pinned here: a well-formed menu comes through as written; an
// href that could run script or leave for another host never reaches an `<a href>`; and
// anything malformed reads as absent — all or nothing for the groups, so a host bug is a
// missing section rather than a menu with holes in it.
import assert from "node:assert/strict";
import test from "node:test";

import { HOST_ATTRS, isHostMode, parseGroups, readHostProps, safeHref, type MenuGroup } from "./host-attributes.ts";

const item = (id: string, extra: Record<string, unknown> = {}) => ({ id, href: `/${id}`, label: id.toUpperCase(), ...extra });

/** An attribute reader over a plain record, standing in for `el.getAttribute.bind(el)`. */
const reader =
  (attrs: Record<string, string>) =>
  (name: string): string | null =>
    attrs[name] ?? null;

// ── safeHref ────────────────────────────────────────────────────────────────

test("a root-relative href is accepted as written", () => {
  assert.equal(safeHref("/x"), "/x");
  assert.equal(safeHref("/en/cabinet/settings?tab=1#a"), "/en/cabinet/settings?tab=1#a");
});

test("an absolute http or https href is accepted as written", () => {
  assert.equal(safeHref("https://sa.evinvest.ltd/places"), "https://sa.evinvest.ltd/places");
  assert.equal(safeHref("http://localhost:3000/"), "http://localhost:3000/");
});

test("surrounding whitespace is trimmed from an accepted href", () => {
  assert.equal(safeHref("  /x  "), "/x");
});

test("a javascript: href is refused", () => {
  assert.equal(safeHref("javascript:alert(1)"), null);
  assert.equal(safeHref("JavaScript:alert(1)"), null);
  assert.equal(safeHref(" javascript:alert(1)"), null);
});

test("a data: href is refused", () => {
  assert.equal(safeHref("data:text/html,<script>alert(1)</script>"), null);
});

test("a protocol-relative href is refused", () => {
  assert.equal(safeHref("//evil.example/x"), null);
});

test("a backslash after the leading slash is refused — browsers read it as protocol-relative", () => {
  assert.equal(safeHref("/\\evil.example"), null);
});

test("a tab or newline that the browser would strip into a protocol-relative href is refused", () => {
  assert.equal(safeHref("/\t/evil.example"), null);
  assert.equal(safeHref("/\n/evil.example"), null);
  assert.equal(safeHref("/\r\n/evil.example"), null);
});

test("a control character or backslash anywhere in the href is refused", () => {
  assert.equal(safeHref("/a\\b"), null);
  assert.equal(safeHref("https://sa.example/\\evil"), null);
  assert.equal(safeHref("/a\u0000b"), null);
  assert.equal(safeHref("/a\u007Fb"), null);
});

test("a bare word, a non-http scheme and a non-string are refused", () => {
  assert.equal(safeHref("settings"), null);
  assert.equal(safeHref("mailto:a@b.c"), null);
  assert.equal(safeHref("ftp://x.example/"), null);
  assert.equal(safeHref(""), null);
  assert.equal(safeHref("   "), null);
  assert.equal(safeHref(42), null);
  assert.equal(safeHref(null), null);
});

test("an href of 2048 characters is accepted and one of 2049 is refused", () => {
  const at = `/${"a".repeat(2047)}`;
  assert.equal(at.length, 2048);
  assert.equal(safeHref(at), at);
  assert.equal(safeHref(`${at}a`), null);
});

// ── parseGroups ─────────────────────────────────────────────────────────────

test("a valid group list comes through with defaults filled in", () => {
  const raw = JSON.stringify([
    { id: "services", label: "Services", items: [item("panel", { current: true }), item("docs", { href: "https://docs.example/", external: true })] },
    { id: "more", items: [item("help")] },
  ]);
  const expected: MenuGroup[] = [
    {
      id: "services",
      label: "Services",
      items: [
        { id: "panel", href: "/panel", label: "PANEL", external: false, current: true },
        { id: "docs", href: "https://docs.example/", label: "DOCS", external: true, current: false },
      ],
    },
    { id: "more", label: null, items: [{ id: "help", href: "/help", label: "HELP", external: false, current: false }] },
  ];
  assert.deepEqual(parseGroups(raw), expected);
});

test("a missing or empty attribute reads as no groups", () => {
  assert.deepEqual(parseGroups(null), []);
  assert.deepEqual(parseGroups(""), []);
});

test("broken JSON reads as no groups rather than throwing", () => {
  assert.deepEqual(parseGroups("[{"), []);
  assert.deepEqual(parseGroups("undefined"), []);
});

test("JSON that is not an array reads as no groups", () => {
  assert.deepEqual(parseGroups(JSON.stringify({ id: "g", items: [item("a")] })), []);
  assert.deepEqual(parseGroups("null"), []);
  assert.deepEqual(parseGroups('"groups"'), []);
});

test("one malformed group empties the whole list", () => {
  const raw = JSON.stringify([{ id: "good", items: [item("a")] }, { id: "", items: [item("b")] }]);
  assert.deepEqual(parseGroups(raw), []);
});

test("one item with an unsafe href empties the whole list", () => {
  const raw = JSON.stringify([
    { id: "good", items: [item("a")] },
    { id: "bad", items: [item("b"), item("c", { href: "javascript:alert(1)" })] },
  ]);
  assert.deepEqual(parseGroups(raw), []);
});

test("an item whose external flag is not a boolean is refused", () => {
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [item("a", { external: "true" })] }])), []);
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [item("a", { external: 1 })] }])), []);
});

test("an item whose current flag is not a boolean is refused", () => {
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [item("a", { current: "yes" })] }])), []);
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [item("a", { current: null })] }])), []);
});

test("an item without an id, href or label is refused", () => {
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [{ href: "/a", label: "A" }] }])), []);
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [{ id: "a", label: "A" }] }])), []);
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [{ id: "a", href: "/a" }] }])), []);
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [null] }])), []);
});

test("a group label that is present but blank or not a string is refused, not dropped", () => {
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", label: "  ", items: [item("a")] }])), []);
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", label: 7, items: [item("a")] }])), []);
});

test("eight groups are accepted and nine are refused", () => {
  const groups = (n: number) => JSON.stringify(Array.from({ length: n }, (_, i) => ({ id: `g${i}`, items: [item("a")] })));
  assert.equal(parseGroups(groups(8)).length, 8);
  assert.deepEqual(parseGroups(groups(9)), []);
});

test("a group of 1 or 16 items is accepted; 0 or 17 is refused", () => {
  const withItems = (n: number) => JSON.stringify([{ id: "g", items: Array.from({ length: n }, (_, i) => item(`i${i}`)) }]);
  assert.equal(parseGroups(withItems(1))[0]?.items.length, 1);
  assert.equal(parseGroups(withItems(16))[0]?.items.length, 16);
  assert.deepEqual(parseGroups(withItems(0)), []);
  assert.deepEqual(parseGroups(withItems(17)), []);
});

test("a group whose items is not an array is refused", () => {
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: item("a") }])), []);
});

test("a label of 254 characters is accepted and one of 255 is refused", () => {
  const at = "L".repeat(254);
  assert.equal(parseGroups(JSON.stringify([{ id: "g", items: [item("a", { label: at })] }]))[0]?.items[0]?.label, at);
  assert.deepEqual(parseGroups(JSON.stringify([{ id: "g", items: [item("a", { label: `${at}L` })] }])), []);
});

// ── isHostMode / readHostProps ──────────────────────────────────────────────

test("host mode is on only for data-mode exactly \"host\"", () => {
  assert.equal(isHostMode(reader({ [HOST_ATTRS.mode]: "host" })), true);
  assert.equal(isHostMode(reader({ [HOST_ATTRS.mode]: "Host" })), false);
  assert.equal(isHostMode(reader({ [HOST_ATTRS.mode]: "cabinet" })), false);
  assert.equal(isHostMode(reader({})), false);
});

test("readHostProps reads every attribute the host sets", () => {
  const props = readHostProps(
    reader({
      "data-name": " Valentina Kuznetsova ",
      "data-email": "v@example.com",
      "data-manage-href": "/settings",
      "data-profile-href": "https://sa.example/profile",
      "data-switch-href": "/switch",
      "data-groups": JSON.stringify([{ id: "g", items: [item("a")] }]),
    }),
  );
  assert.deepEqual(props, {
    name: "Valentina Kuznetsova",
    email: "v@example.com",
    manageHref: "/settings",
    profileHref: "https://sa.example/profile",
    switchHref: "/switch",
    groups: [{ id: "g", label: null, items: [{ id: "a", href: "/a", label: "A", external: false, current: false }] }],
  });
});

test("readHostProps reads absent attributes as nulls and no groups", () => {
  assert.deepEqual(readHostProps(reader({})), {
    name: null,
    email: null,
    manageHref: null,
    profileHref: null,
    switchHref: null,
    groups: [],
  });
});

test("a blank name or email reads as absent", () => {
  const props = readHostProps(reader({ "data-name": "   ", "data-email": "" }));
  assert.equal(props.name, null);
  assert.equal(props.email, null);
});

test("an unsafe link attribute reads as absent without touching the others", () => {
  const props = readHostProps(
    reader({ "data-manage-href": "javascript:alert(1)", "data-profile-href": "//evil.example", "data-switch-href": "/switch" }),
  );
  assert.equal(props.manageHref, null);
  assert.equal(props.profileHref, null);
  assert.equal(props.switchHref, "/switch");
});
