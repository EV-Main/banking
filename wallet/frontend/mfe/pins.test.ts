// Run with `node --test --experimental-strip-types mfe/pins.test.ts`.
//
// `npm run --silent mfe:pins` prints the line `@evinvest/wallet` parses with `parseWalletPins`:
// exactly one line, `wallet.js=<hex>,wallet.css=<hex>`, in that order, each the sha256 of the
// file's bytes — the expected digests below were taken with `shasum -a 256` on the same
// fixture bytes, not computed by the test — and a missing bundle is a failure, never a line
// with a pin missing. `wallet` as an argument means the same; any other argument is a failure.
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

let root: string;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "wallet-pins-"));
  await mkdir(join(root, "mfe"));
  await mkdir(join(root, "public", "mfe"), { recursive: true });
  await copyFile(SCRIPT, join(root, "mfe", "pins.mjs"));
});

afterEach(async () => {
  await rm(root, { recursive: true, force: true });
});

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

test("without an argument the same line is printed", async () => {
  await walletBundles({ js: WALLET_JS, css: WALLET_CSS });

  const { stdout } = await pinsWith();

  assert.equal(stdout, `wallet.js=${WALLET_JS_SHA256},wallet.css=${WALLET_CSS_SHA256}\n`);
});

test("a changed byte changes its pin and only its pin", async () => {
  await walletBundles({ js: WALLET_JS, css: ".wallet{color:blue}" });

  const { stdout } = await pinsWith();

  assert.match(stdout, new RegExp(`^wallet\\.js=${WALLET_JS_SHA256},wallet\\.css=(?!${WALLET_CSS_SHA256})[0-9a-f]{64}\\n$`));
});

test("a missing wallet bundle fails with the build hint and prints no pins", async () => {
  await walletBundles({ css: WALLET_CSS });

  const failed = await failure(pinsWith());

  assert.notEqual(failed.code, 0);
  assert.equal(failed.stdout, "");
  assert.match(failed.stderr, /public\/mfe\/wallet\/wallet\.js is missing — run `npm run build:mfe` first/);
});

test("an unknown argument fails and prints no pins", async () => {
  await walletBundles({ js: WALLET_JS, css: WALLET_CSS });

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
