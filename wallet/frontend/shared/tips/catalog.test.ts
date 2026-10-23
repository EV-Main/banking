// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The catalog used to carry its own copy, and these assertions read it straight off the
// entries. The copy now lives in the message catalogue under `tips.<key>.title` /
// `tips.<key>.body`, so the same assertions point there instead of being deleted: the
// risk they cover did not go away, it moved. In fact one got sharper — a tip key with no
// English entry is now possible in a way it was not before (a `TipAnchor` still compiles,
// and the missing key only shows up as raw `tips.foo.title` on screen), so "every key has
// both halves" is the first thing checked.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { tips, type TipEntry } from "./catalog.ts";

const ROLES = new Set(["investor", "operator", "admin", "owner"]);
const entries = Object.entries(tips) as [string, TipEntry][];

// Read rather than import: the runner strips types but does not apply the tsconfig's JSON
// module resolution, and this only needs the flat `key → English` map.
const en = JSON.parse(
  readFileSync(new URL("../../messages/en/common.json", import.meta.url), "utf8"),
) as Record<string, string>;

const copyOf = (key: string) => ({ title: en[`tips.${key}.title`], body: en[`tips.${key}.body`] });

test("catalog is non-empty", () => {
  assert.ok(entries.length > 0, "the catalog should have entries");
});

test("every entry has a valid type", () => {
  for (const [key, e] of entries) {
    assert.ok(e.type === "input" || e.type === "section", `${key}: bad type ${e.type}`);
  }
});

test("every entry has English title and body copy", () => {
  for (const [key] of entries) {
    const { title, body } = copyOf(key);
    assert.ok(typeof title === "string" && title.trim().length > 0, `${key}: missing tips.${key}.title in messages/en/common.json`);
    assert.ok(typeof body === "string" && body.trim().length > 0, `${key}: missing tips.${key}.body in messages/en/common.json`);
  }
});

test("no orphaned tip copy in the catalogue", () => {
  const known = new Set(entries.map(([key]) => key));
  for (const key of Object.keys(en)) {
    const m = /^tips\.(.+)\.(title|body)$/.exec(key);
    // `tips.a11y.about` is the trigger's aria-label pattern, not an entry's copy.
    if (!m || m[1] === "a11y") continue;
    assert.ok(known.has(m[1]!), `${key}: copy for a tip that is not in the catalog`);
  }
});

test("copy stays concise and clean", () => {
  for (const [key] of entries) {
    const { title, body } = copyOf(key);
    if (title === undefined || body === undefined) continue; // reported above
    assert.ok(title.length <= 48, `${key}: title too long (${title.length})`);
    assert.ok(body.length <= 320, `${key}: body too long (${body.length})`);
    assert.ok(!/\s{2,}/.test(title + " " + body), `${key}: double spaces in copy`);
    assert.equal(title, title.trim(), `${key}: title has edge whitespace`);
    assert.equal(body, body.trim(), `${key}: body has edge whitespace`);
  }
});

test("role gates use only known platform roles", () => {
  for (const [key, e] of entries) {
    if (!e.roles) continue;
    assert.ok(e.roles.length > 0, `${key}: empty roles array — omit it instead`);
    for (const r of e.roles) assert.ok(ROLES.has(r), `${key}: unknown role ${r}`);
  }
});

test("admin.* tips are operator-gated; investor tips are not", () => {
  for (const [key, e] of entries) {
    if (key.startsWith("admin.")) {
      assert.ok(e.roles && !e.roles.includes("investor"), `${key}: admin tip must be role-gated`);
    } else {
      assert.equal(e.roles, undefined, `${key}: investor-facing tip should not be role-gated`);
    }
  }
});

test("keys are dot-namespaced by surface", () => {
  for (const [key] of entries) {
    assert.match(key, /^[a-z][a-z0-9-]*(\.[a-z0-9-]+)+$/, `${key}: not a dot.kebab key`);
  }
});
