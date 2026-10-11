// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Initials and the email-derived name are what the account chip paints first, and the
// initials are also written to localStorage. What is pinned here: a letter is a whole
// grapheme — an emoji or an astral letter is never cut into a lone surrogate (rendered as
// U+FFFD and not well-formed for storage or `encodeURIComponent`), and a decomposed accent
// stays on its letter.
import assert from "node:assert/strict";
import test from "node:test";

import { translator } from "@evinvest/i18n";

import { displayName, initialsOf, initialsOfName } from "./identity.ts";

/** The translator only supplies the worded fallback for a missing email; no catalogue needed. */
const t = translator({}, "en");

test("email initials keep an emoji and an astral letter whole", () => {
  const initials = initialsOf("😀x.𝒜b@x.io");

  assert.equal(initials, "😀𝒜");
  assert.equal(initials.isWellFormed(), true);
});

test("name initials uppercase a decomposed accented letter with its accent", () => {
  // "e" + U+0301 COMBINING ACUTE ACCENT, as macOS file names and some keyboards produce it.
  const initials = initialsOfName("émile zola", null);

  assert.equal(initials, "ÉZ");
});

test("name initials uppercase a precomposed accented letter", () => {
  assert.equal(initialsOfName("émile zola", null), "ÉZ");
});

test("name initials keep an emoji whole", () => {
  const initials = initialsOfName("😀 Smile", "v@example.com");

  assert.equal(initials, "😀S");
  assert.equal(initials.isWellFormed(), true);
});

test("a blank name falls back to the email initials", () => {
  assert.equal(initialsOfName("  ", "ada.lovelace@example.com"), "AL");
});

test("the display name takes an astral letter whole as the last initial", () => {
  const name = displayName("ada.𝒜vel@x", t);

  assert.equal(name, "Ada 𝒜.");
  assert.equal(name.isWellFormed(), true);
});

test("the display name capitalises an emoji-led handle without splitting it", () => {
  const name = displayName("😀x.zola@x", t);

  assert.equal(name, "😀x Z.");
  assert.equal(name.isWellFormed(), true);
});
