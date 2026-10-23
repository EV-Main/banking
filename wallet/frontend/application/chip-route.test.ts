// Run with `node --test --experimental-strip-types application/chip-route.test.ts`.
//
// The wallet host routes an account chip's `ev:navigate` itself only when the event came out of a
// chip element and the destination is one of its own localized pages, `/{locale}/cabinet…`.
// Everything else stays a full load: the BFF and auth endpoints, other zones, the unprefixed
// `/cabinet/…` only the proxy can repair, and anything that is not a path-absolute string on
// this origin — the detail is outside input, written by whatever dispatched the event.
import assert from "node:assert/strict";
import test from "node:test";

import { zoneRouteFor, fromChip } from "./chip-route.ts";

const at = (href: unknown) => zoneRouteFor({ href });

// ── fromChip ────────────────────────────────────────────────────────────────

test("an event whose path crosses the chip element came from the chip", () => {
  const path = [{ localName: "a" }, { localName: "div" }, { localName: "mfe-cabinet-account-chip" }, { localName: "body" }, {}];

  assert.equal(fromChip(path as unknown as EventTarget[]), true);
});

test("an event from anywhere else, or with only document and window in its path, did not", () => {
  const path = [{ localName: "a" }, { localName: "mfe-cabinet-account-chips" }, {}];

  assert.equal(fromChip(path as unknown as EventTarget[]), false, "a look-alike tag");
  assert.equal(fromChip([new EventTarget()]), false, "no localName at all");
  assert.equal(fromChip([]), false, "empty path");
});

// ── zoneRouteFor: taken ──────────────────────────────────────────────────

test("a localized zone page is routed with its query and hash", () => {
  assert.equal(at("/en/cabinet/settings?tab=security#top"), "/en/cabinet/settings?tab=security#top");
});

test("the profile page and the wallet host home are routed", () => {
  assert.equal(at("/en/cabinet/profile"), "/en/cabinet/profile");
  assert.equal(at("/en/cabinet"), "/en/cabinet");
  assert.equal(at("/ru/cabinet"), "/ru/cabinet");
});

// ── zoneRouteFor: left to the browser ────────────────────────────────────

test("an unprefixed zone path is left to the proxy's locale repair", () => {
  assert.equal(at("/cabinet/wallet"), null);
});

test("BFF and auth endpoints are never routed", () => {
  assert.equal(at("/api/auth/login"), null, "auth");
  assert.equal(at("/api/auth/login?returnTo=%2Fen%2Fcabinet"), null, "Switch account");
  assert.equal(at("/cabinet/api/users"), null, "BFF");
});

test("other zones and look-alike segments are not the wallet host", () => {
  assert.equal(at("/en/cabinets"), null, "a sibling route");
  assert.equal(at("/"), null, "the site root");
  assert.equal(at("/zz/cabinet/x"), null, "not a locale");
});

test("anything that leaves this origin is refused", () => {
  assert.equal(at("//evil.com/x"), null, "protocol-relative");
  // Refused as protocol-relative, not by host: even one that resolves onto the parser's own base.
  assert.equal(at("//cabinet.invalid/en/cabinet"), null, "protocol-relative onto the base host");
  assert.equal(at("/\\evil.com/en/cabinet"), null, "backslash read as a slash");
  assert.equal(at("https://evil.com/en/cabinet"), null, "absolute URL");
  assert.equal(at("en/cabinet"), null, "relative path");
});

test("a detail that is not { href: string } is refused", () => {
  assert.equal(at(42), null, "numeric href");
  assert.equal(at(undefined), null, "missing href");
  assert.equal(zoneRouteFor({}), null, "no href key");
  assert.equal(zoneRouteFor(null), null, "null detail");
  assert.equal(zoneRouteFor(undefined), null, "undefined detail");
  assert.equal(zoneRouteFor("/en/cabinet"), null, "a bare string");
});
