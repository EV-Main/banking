// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// The deposit QR's encoder is downloaded on demand. These pin the three ways that can end
// for the slot beside the address: the encoder is already here (drawn at once, no
// suspension and no skeleton frame), still downloading (the skeleton's exact box), or never
// arriving (a placeholder in the same box, reported — never an error that takes the
// address down with it). Rendered with `react-dom/server`, so no DOM is needed; the plate's
// download is a fake the test lands or fails by hand.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { DEFAULT_LOCALE, type Locale } from "@evinvest/i18n";
import { resolveCatalogue, type TranslatedCatalogue } from "@evinvest/i18n/policy";
import { I18nProvider } from "@evinvest/i18n/react";
import { createElement, type ReactNode } from "react";
import { renderToString } from "react-dom/server";
import { prerenderToNodeStream } from "react-dom/static";

import { chunkLoadError, fakeDownload } from "../../../shared/__tests__/fake-download.ts";
import { reportsReach, sentReports } from "../../../shared/__tests__/fake-sentry-reports.ts";
import { installModuleHooks } from "../../../shared/__tests__/react-hook-harness.ts";

type QrModule = typeof import("./deposit-qr.tsx");

installModuleHooks({
  "@/views/wallet/ui/deposit-qr-plate": new URL("./__tests__/deposit-qr-plate.stub.ts", import.meta.url).href,
  "@sentry/react": new URL("../../../shared/__tests__/fake-sentry.mjs", import.meta.url).href,
});

const ADDRESS = "TQn9Y2khEsLJW1ChVWFMSMeRDow5KcbLSE";

let cases = 0;
/** A fresh copy of the QR module — its loaded-encoder cache and lazy start empty — and the
 *  plate download that copy makes. */
const freshQr = async () => {
  cases += 1;
  const plate = fakeDownload("wallet.qr-plate", String(cases));
  const mod: QrModule = await import(`./deposit-qr.tsx?case=${cases}`);
  return { mod, plate };
};

/** Lets every continuation already scheduled run (one macrotask turn); not a timer. */
const drain = () => new Promise<void>((resolve) => setImmediate(resolve));

const catalogue = (locale: Locale) => JSON.parse(readFileSync(new URL(`../../../messages/${locale}/common.json`, import.meta.url), "utf8"));
const messages = (locale: Locale) => (locale === DEFAULT_LOCALE ? catalogue(locale) : resolveCatalogue(locale, catalogue(DEFAULT_LOCALE), catalogue(locale) as TranslatedCatalogue).messages);
const inLocale = (locale: Locale, children: ReactNode) =>
  // eslint-disable-next-line react/no-children-prop -- no JSX in a .ts test, and the provider's props type `children` as required
  createElement(I18nProvider, { locale, messages: messages(locale), children });

/** Renders to completion — every Suspense boundary resolved — and collects render errors. */
async function settledHtml(tree: ReactNode): Promise<{ html: string; errors: unknown[] }> {
  const errors: unknown[] = [];
  const { prelude } = await prerenderToNodeStream(tree, { onError: (error) => void errors.push(error) });
  let html = "";
  for await (const chunk of prelude) html += String(chunk);
  return { html, errors };
}

test("an encoder that has already arrived is drawn at once, with no skeleton frame", async () => {
  const { mod, plate } = await freshQr();
  mod.preloadDepositQr();
  await plate.arrive();
  await drain();

  const html = renderToString(inLocale("en", createElement(mod.DepositQr, { value: ADDRESS })));

  assert.match(html, /<svg/);
  assert.doesNotMatch(html, /data-slot="skeleton"/);
});

test("while the encoder downloads the slot holds the address skeleton's box", async () => {
  const { mod } = await freshQr();
  mod.preloadDepositQr();

  const html = renderToString(inLocale("en", createElement(mod.DepositQr, { value: ADDRESS })));

  assert.match(html, /data-slot="skeleton"[^>]*class="[^"]*size-40[^"]*lg:size-45/);
  assert.doesNotMatch(html, /<svg/);
});

test("an encoder that never arrives leaves a placeholder in the same box, not an error", async () => {
  const { mod, plate } = await freshQr();
  mod.preloadDepositQr();
  await plate.fail(chunkLoadError());
  await drain();

  const { html, errors } = await settledHtml(inLocale("en", createElement(mod.DepositQr, { value: ADDRESS })));

  assert.deepEqual(errors, []);
  assert.match(html, /QR code unavailable — use the address below\./);
  assert.match(html, /class="[^"]*size-40[^"]*lg:size-45/);
});

test("the missing encoder is reported once, naming the deposit QR", { timeout: 10_000 }, async () => {
  const { mod, plate } = await freshQr();
  const before = sentReports().length;
  const refused = chunkLoadError();
  mod.preloadDepositQr();
  await plate.fail(refused);
  await drain();

  await settledHtml(inLocale("en", createElement(mod.DepositQr, { value: ADDRESS })));
  await reportsReach(before + 1);
  await drain();

  assert.deepEqual(sentReports().slice(before), [{ error: refused, hint: { extra: { where: "wallet: deposit QR encoder" } } }]);
});

test("the placeholder is in the reader's language", async () => {
  const { mod, plate } = await freshQr();
  mod.preloadDepositQr();
  await plate.fail(chunkLoadError());
  await drain();

  const { html } = await settledHtml(inLocale("ru", createElement(mod.DepositQr, { value: ADDRESS })));

  assert.match(html, /QR-код недоступен — используйте адрес ниже\./);
});
