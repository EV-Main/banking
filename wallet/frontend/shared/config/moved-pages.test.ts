import assert from "node:assert/strict";
import { test } from "node:test";

import { renamedPaymentTarget } from "./moved-pages.ts";

const id = "4f0c2d1e-0000-4000-8000-0123456789ab";

test("an old payment notice link lands on the payment page, locale and query kept", () => {
  assert.equal(
    renamedPaymentTarget(`/en/cabinet/wallet/action/payment/${id}`, "?scope=invest"),
    `/en/cabinet/wallet/payment/${id}?scope=invest`,
  );
});

test("an old payment notice link without a locale stays without one", () => {
  assert.equal(renamedPaymentTarget(`/cabinet/wallet/action/payment/${id}`, ""), `/cabinet/wallet/payment/${id}`);
});

test("anything but a plain payment id is not redirected", () => {
  for (const path of [
    "/en/cabinet/wallet/action/payment/",
    "/en/cabinet/wallet/action/payment/a.b",
    "/en/cabinet/wallet/action/payment/a%2F%2Fevil.example",
    "/en/cabinet/wallet/action/payment/a/b",
    `/en/cabinet/wallet/action/payment/${"a".repeat(129)}`,
    "/en/cabinet/wallet/action/withdraw",
    `/en/cabinet/wallet/payment/${id}`,
    `/en/wallet/action/payment/${id}`,
  ]) {
    assert.equal(renamedPaymentTarget(path, ""), null, path);
  }
});
