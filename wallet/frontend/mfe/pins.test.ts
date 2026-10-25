// Run with `node --test --experimental-strip-types mfe/pins.test.ts`.
//
// `npm run --silent mfe:pins` prints the line that goes, as is, into the panel's
// PANEL_CABINET_MFE_PINS; the panel fail-closes on any chip byte that does not match it. What
// is pinned here: exactly one line, `account-chip.js=<hex>,account-chip.css=<hex>`, in that
// order, each the sha256 of the file's bytes — the expected digests below were taken with
// `shasum -a 256` on the same fixture bytes, not computed by the test — and a missing bundle
// is a failure, never a line with a pin missing. `-- wallet` prints the same shape for the
// wallet bundle; any other argument is a failure.
//
// The script reads `../public/mfe/` relative to itself, so each test runs a copy of it next
// to fixture bundles in a temporary directory; the repository's own public/ is not touched.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { copyFile, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test, { afterEach, beforeEach } from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const run = promisify(execFile);
const SCRIPT = fileURLToPath(new URL("./pins.mjs", import.meta.url));

const JS = "export const chip = 1;\n";
const CSS = ".chip{color:red}\n";
// $ printf 'export const chip = 1;\n' | shasum -a 256
const JS_SHA256 = "dab1eac5ec339bc9246d6675ba67f29e3aa77dc6eb95b3fdcffb2f4da995f56b";
// $ printf '.chip{color:red}\n' | shasum -a 256
const CSS_SHA256 = "1f5c0c411b6988df6c9dda8d97a614417ddf09d6ffc199b0e84572884582f746";

let root: string;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "chip-pins-"));
  await mkdir(join(root, "mfe"));
  await mkdir(join(root, "public", "mfe"), { recursive: true });
  await copyFile(SCRIPT, join(root, "mfe", "pins.mjs"));
});

afterEach(async () => {
  await rm(root, { recursive: true, force: true });
});

async function bundles(files: { js?: string; css?: string }): Promise<void> {
  if (files.js !== undefined) await writeFile(join(root, "public", "mfe", "account-chip.js"), files.js);
  if (files.css !== undefined) await writeFile(join(root, "public", "mfe", "account-chip.css"), files.css);
}

function pins(): Promise<{ stdout: string; stderr: string }> {
  return run(process.execPath, [join(root, "mfe", "pins.mjs")]);
}

test("prints one line with the js and css sha256 pins, js first", async () => {
  await bundles({ js: JS, css: CSS });

  const { stdout } = await pins();

  assert.equal(stdout, `account-chip.js=${JS_SHA256},account-chip.css=${CSS_SHA256}\n`);
});

test("a changed byte changes its pin and only its pin", async () => {
  await bundles({ js: JS, css: ".chip{color:red}" });

  const { stdout } = await pins();

  assert.match(stdout, new RegExp(`^account-chip\\.js=${JS_SHA256},account-chip\\.css=(?!${CSS_SHA256})[0-9a-f]{64}\\n$`));
});

test("a missing bundle fails with the build hint and prints no pins", async () => {
  await bundles({ js: JS });

  const failed = await pins().then(
    () => assert.fail("pins.mjs succeeded without account-chip.css"),
    (error: unknown) => error as { code: number; stdout: string; stderr: string },
  );

  assert.notEqual(failed.code, 0);
  assert.equal(failed.stdout, "");
  assert.match(failed.stderr, /public\/mfe\/account-chip\.css is missing — run `npm run build:mfe` first/);
});

// ── wallet (`mfe:pins -- wallet`): the line `@evinvest/wallet` parses with `parseWalletPins`.

const WALLET_JS = "export const wallet = 1;\n";
const WALLET_CSS = ".wallet{color:blue}\n";
// $ printf 'export const wallet = 1;\n' | shasum -a 256
const WALLET_JS_SHA256 = "936107adb3e970a54546140d379df936f0e7da01b374c68485a72aefa9c0bb24";
// $ printf '.wallet{color:blue}\n' | shasum -a 256
const WALLET_CSS_SHA256 = "310767756e9d1fba6550eafdef0ce360665cfae9870a847332847f080316d8ef";

async function walletBundles(files: { js?: string; css?: string }): Promise<void> {
  await mkdir(join(root, "public", "mfe", "wallet"), { recursive: true });
  if (files.js !== undefined) await writeFile(join(root, "public", "mfe", "wallet", "wallet.js"), files.js);
  if (files.css !== undefined) await writeFile(join(root, "public", "mfe", "wallet", "wallet.css"), files.css);
}

function pinsWith(...args: string[]): Promise<{ stdout: string; stderr: string }> {
  return run(process.execPath, [join(root, "mfe", "pins.mjs"), ...args]);
}

function failure(promise: Promise<unknown>): Promise<{ code: number; stdout: string; stderr: string }> {
  return promise.then(
    () => assert.fail("pins.mjs succeeded"),
    (error: unknown) => error as { code: number; stdout: string; stderr: string },
  );
}

test("wallet prints one line with the wallet js and css pins, js first, named relative to the served directory", async () => {
  await walletBundles({ js: WALLET_JS, css: WALLET_CSS });

  const { stdout } = await pinsWith("wallet");

  assert.equal(stdout, `wallet.js=${WALLET_JS_SHA256},wallet.css=${WALLET_CSS_SHA256}\n`);
});

test("wallet pins the wallet bundle, not the chip beside it", async () => {
  await bundles({ js: JS, css: CSS });
  await walletBundles({ js: WALLET_JS, css: WALLET_CSS });

  const { stdout } = await pinsWith("wallet");

  assert.equal(stdout, `wallet.js=${WALLET_JS_SHA256},wallet.css=${WALLET_CSS_SHA256}\n`);
});

test("without an argument the chip line is printed even when the wallet bundle is there", async () => {
  await bundles({ js: JS, css: CSS });
  await walletBundles({ js: WALLET_JS, css: WALLET_CSS });

  const { stdout } = await pins();

  assert.equal(stdout, `account-chip.js=${JS_SHA256},account-chip.css=${CSS_SHA256}\n`);
});

test("a missing wallet bundle fails with the build hint and prints no pins", async () => {
  await walletBundles({ css: WALLET_CSS });

  const failed = await failure(pinsWith("wallet"));

  assert.notEqual(failed.code, 0);
  assert.equal(failed.stdout, "");
  assert.match(failed.stderr, /public\/mfe\/wallet\/wallet\.js is missing — run `npm run build:mfe` first/);
});

test("an unknown argument fails and prints no pins", async () => {
  await bundles({ js: JS, css: CSS });

  const failed = await failure(pinsWith("chip"));

  assert.notEqual(failed.code, 0);
  assert.equal(failed.stdout, "");
  assert.match(failed.stderr, /unknown argument: chip/);
});

test("a second argument after wallet fails and prints no pins", async () => {
  await walletBundles({ js: WALLET_JS, css: WALLET_CSS });

  const failed = await failure(pinsWith("wallet", "extra"));

  assert.notEqual(failed.code, 0);
  assert.equal(failed.stdout, "");
});
