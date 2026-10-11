// Prints the sha256 pins of the built account-chip bundle, one line, nothing else — the value
// of the panel's PANEL_CABINET_MFE_PINS. The panel serves these files from its own origin, next
// to its HttpOnly session, so it fail-closes on any byte that does not match a pin: a chip
// release reaches the panel only once this line is in its env (gitops), not on its own.
//
// Run after `npm run build:mfe`, on the very files that ship (uncompressed, as the panel
// hashes them):
//   $ npm run --silent mfe:pins
//   account-chip.js=3f1c…e9,account-chip.css=a07b…41
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

// Order and names are the panel's: it parses `file=hex` pairs, 64 lowercase hex each.
const FILES = ["account-chip.js", "account-chip.css"];
const DIR = new URL("../public/mfe/", import.meta.url);

const pins = [];
for (const file of FILES) {
  let body;
  try {
    body = await readFile(new URL(file, DIR));
  } catch (cause) {
    throw new Error(`public/mfe/${file} is missing — run \`npm run build:mfe\` first`, { cause });
  }
  pins.push(`${file}=${createHash("sha256").update(body).digest("hex")}`);
}
process.stdout.write(`${pins.join(",")}\n`);
