// Prints the sha256 pins of a built bundle, one line, nothing else. Run after `npm run
// build:mfe`, on the very files that ship (uncompressed, as the consumers hash them).
//
// The wallet elements' bundle: the line `@evinvest/wallet` reads (`parseWalletPins`). Names are
// relative to the served directory https://evinvest.ltd/wallet/mfe/, and the SDK checks one
// module: the build emits no chunks. `wallet` as an argument is accepted and means the same.
//   $ npm run --silent mfe:pins
//   wallet.js=…,wallet.css=…
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

// Order and names are the consumers': each parses `file=hex` pairs, 64 lowercase hex each.
const dir = "public/mfe/wallet/";
const files = ["wallet.js", "wallet.css"];

const args = process.argv.slice(2);
if (args.length > 1 || (args.length === 1 && args[0] !== "wallet")) {
  process.stderr.write(`usage: mfe:pins [wallet] — unknown argument: ${args.join(" ")}\n`);
  process.exit(2);
}

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
