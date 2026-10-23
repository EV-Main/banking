// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { VENDOR_SESSION_HOST, providerUrl } from "./provider-url.ts";

const NONE: readonly string[] = [];
const OK = `https://${VENDOR_SESSION_HOST}/session/abc`;

test("the vendor's session host needs no configuration", () => {
  // The regression this file exists for: the allowlist used to come from
  // NEXT_PUBLIC_KYC_PROVIDER_HOST alone and degrade to the scheme check when unset — which
  // is what every image built by flake.nix did, production included.
  assert.equal(providerUrl(OK, NONE), OK);
  assert.equal(VENDOR_SESSION_HOST, "verify.didit.me", "the API host `verification.didit.me` is a different name");
});

test("a non-https scheme is refused on any host", () => {
  assert.equal(providerUrl("javascript:alert(1)", NONE), null);
  assert.equal(providerUrl("data:text/html,<script>alert(1)</script>", NONE), null);
  assert.equal(providerUrl(`http://${VENDOR_SESSION_HOST}/session/abc`, NONE), null);
});

test("an https URL on another host is refused", () => {
  assert.equal(providerUrl("https://evil.example/session/abc", NONE), null);
  // A suffix match would let `notverify.didit.me` and `verify.didit.me.evil.example` through.
  assert.equal(providerUrl("https://notverify.didit.me/s", NONE), null);
  assert.equal(providerUrl("https://verify.didit.me.evil.example/s", NONE), null);
  // Userinfo: the host of this URL is `evil.example`, whatever it reads like.
  assert.equal(providerUrl(`https://${VENDOR_SESSION_HOST}@evil.example/s`, NONE), null);
});

test("the port is part of the host", () => {
  assert.equal(providerUrl(`https://${VENDOR_SESSION_HOST}:8443/s`, NONE), null);
  assert.equal(providerUrl(`https://${VENDOR_SESSION_HOST}:8443/s`, ["verify.didit.me:8443"]), `https://${VENDOR_SESSION_HOST}:8443/s`);
});

test("the host comparison ignores case on both sides", () => {
  assert.equal(providerUrl("https://VERIFY.Didit.ME/s", NONE), "https://VERIFY.Didit.ME/s");
  assert.equal(providerUrl("https://Sandbox.Didit.Example/s", ["sandbox.didit.example"]), "https://Sandbox.Didit.Example/s");
});

test("extra hosts add to the vendor's, they do not replace it", () => {
  // An operator who points the plane at a sandbox (concierge admits the origin of
  // DIDIT_BASE_URL beside its own constant) sets this and keeps production working.
  assert.equal(providerUrl("https://sandbox.didit.example/s", ["sandbox.didit.example"]), "https://sandbox.didit.example/s");
  assert.equal(providerUrl(OK, ["sandbox.didit.example"]), OK);
  assert.equal(providerUrl("https://other.didit.example/s", ["sandbox.didit.example"]), null);
});

test("the cabinet's own host is admitted, for KYC_STUB", () => {
  // The stub provider hands back a page on the cabinet's own origin, so a same-origin
  // redirect is not a hand-off to a third party and there is nothing to protect against.
  assert.equal(providerUrl("https://evinvest.test/cabinet/kyc/stub", NONE, "evinvest.test"), "https://evinvest.test/cabinet/kyc/stub");
  assert.equal(providerUrl("https://evinvest.test/cabinet/kyc/stub", NONE), null, "…and only when it IS this cabinet");
  assert.equal(providerUrl("https://evil.example/s", NONE, "evinvest.test"), null);
});

test("a missing or unparseable URL is refused", () => {
  assert.equal(providerUrl(null, NONE), null);
  assert.equal(providerUrl("", NONE), null);
  assert.equal(providerUrl("not a url", NONE), null);
});
