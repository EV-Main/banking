// Run with `npm run test` (Node's built-in runner, native type-stripping).
import assert from "node:assert/strict";
import test from "node:test";

import { displayAddress, tonFriendlyAddress } from "./ton-address.ts";

// The example address from the TON docs (docs.ton.org, "Smart Contract Addresses").
// Both forms cross-checked against an independent CRC16/XMODEM implementation
// (check value 0x31c3 for "123456789") during implementation.
const RAW = "0:ca6e321c7cce9ecedf0a8ca2492ec8592494aa5fb5ce0387dff96ef6af982a3e";

test("the TON docs vector encodes to both friendly forms", () => {
  assert.equal(tonFriendlyAddress(RAW, { bounceable: true }), "EQDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPrHF");
  // Non-bounceable is the default — deposit display must not use the EQ… form.
  assert.equal(tonFriendlyAddress(RAW), "UQDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPuwA");
  assert.equal(tonFriendlyAddress(RAW, { bounceable: false }), tonFriendlyAddress(RAW));
});

test("the testnet flag (0x80) yields the test-only forms", () => {
  // TEP-2 test-only tags: 0xD1 non-bounceable-test (`0Q…`), 0x91 bounceable-test (`kQ…`) —
  // a different checksummed string than the mainnet twin, so a testnet rail can't display a
  // mainnet-tagged address. Prefixes cross-checked against TON's test-only address forms.
  assert.equal(tonFriendlyAddress(RAW, { testnet: true }), "0QDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPleK");
  assert.equal(tonFriendlyAddress(RAW, { bounceable: true, testnet: true }), "kQDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPgpP");
  // A false/absent flag is the mainnet form (unchanged).
  assert.equal(tonFriendlyAddress(RAW, { testnet: false }), tonFriendlyAddress(RAW));
});

test("workchain -1 encodes as int8 0xff", () => {
  const friendly = tonFriendlyAddress(`-1:${RAW.slice(2)}`);
  assert.ok(friendly);
  // 0x51 0xff … base64url-encodes to a "Uf8"-class prefix ("Uf_" once url-safe).
  assert.equal(friendly.startsWith("Uf_"), true);
});

test("malformed inputs return null", () => {
  assert.equal(tonFriendlyAddress("UQDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPuwA"), null); // already friendly
  assert.equal(tonFriendlyAddress("0:ca6e321c"), null); // short hex
  assert.equal(tonFriendlyAddress("0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B"), null); // EVM address
  assert.equal(tonFriendlyAddress(""), null);
  assert.equal(tonFriendlyAddress(`300:${RAW.slice(2)}`), null); // workchain outside int8
});

test("displayAddress rewrites only the TON rail", () => {
  const EVM = "0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B";
  // The raw form the hub stores never reaches an operator/user screen as-is.
  assert.equal(displayAddress("ton", RAW), "UQDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPuwA");
  assert.equal(displayAddress("ton", RAW, { testnet: true }), "0QDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPleK");
  // Every other rail already stores its user-facing form — pass through untouched, and
  // an EVM address must never be mistaken for a convertible TON one.
  assert.equal(displayAddress("bep20", EVM), EVM);
  assert.equal(displayAddress("ton", EVM), EVM);
  // An already-friendly TON string (a user-supplied withdrawal destination) survives.
  assert.equal(displayAddress("ton", "UQDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPuwA"), "UQDKbjIcfM6ezt8KjKJJLshZJJSqX7XOA4ff-W72r5gqPuwA");
  // The empty "rail unavailable" sentinel stays empty, so callers can still test it.
  assert.equal(displayAddress("ton", ""), "");
});

test("output is exactly 48 chars of unpadded base64url", () => {
  const friendly = tonFriendlyAddress(RAW);
  assert.ok(friendly);
  assert.equal(friendly.length, 48);
  assert.match(friendly, /^[A-Za-z0-9_-]{48}$/);
});
