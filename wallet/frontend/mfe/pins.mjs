// Prints the sha256 pins of a built bundle, one line, nothing else. Run after `npm run
// build:mfe`, on the very files that ship (uncompressed, as the consumers hash them).
//
// Without an argument: the account chip, the value of the panel's PANEL_CABINET_MFE_PINS. The
// panel serves these files from its own origin, next to its HttpOnly session, so it fail-closes
// on any byte that does not match a pin: a chip release reaches the panel only once this line
// is in its env (gitops), not on its own.
//   $ npm run --silent mfe:pins
//   account-chip.js=3f1c…e9,account-chip.css=a07b…41
//
// With `wallet`: the wallet elements' bundle, the line `@evinvest/wallet` reads
// (`parseWalletPins`). Names are relative to the served directory
// https://evinvest.ltd/wallet/mfe/, and the SDK checks one module: the build emits no chunks.
//   $ npm run --silent mfe:pins -- wallet
//   wallet.js=…,wallet.css=…
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

// Order and names are the consumers': each parses `file=hex` pairs, 64 lowercase hex each.
const BUNDLES = {
  chip: { dir: "public/mfe/", files: ["account-chip.js", "account-chip.css"] },
  wallet: { dir: "public/mfe/wallet/", files: ["wallet.js", "wallet.css"] },
};

const args = process.argv.slice(2);
const name = args.length === 0 ? "chip" : args.length === 1 && args[0] === "wallet" ? "wallet" : null;
if (name === null) {
  process.stderr.write(`usage: mfe:pins [wallet] — unknown argument: ${args.join(" ")}\n`);
  process.exit(2);
}

const { dir, files } = BUNDLES[name];
const base = new URL(`../${dir}`, import.meta.url);
const pins = [];
for (const file of files) {
  let body;
  try {
    body = await readFile(new URL(file, base));
  } catch (cause) {
    throw new Error(`${dir}${file} is missing — run \`npm run build:mfe\` first`, { cause });
  }
  pins.push(`${file}=${createHash("sha256").update(body).digest("hex")}`);
}
process.stdout.write(`${pins.join(",")}\n`);
