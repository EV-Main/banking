// CI gate for the sliced catalogue: every page renders its tree inside its own
// `<I18nScope>`.
//
// The root provider carries only the shell's keys (`shellMessages`); a page's own
// keys reach its client islands through nothing but that scope. A page that forgets
// it, or names another page's entry, still renders — every `t(key, English)` falls
// back to the call site's English — so the reader of another language gets English
// without a sound, and neither `tsc` nor `evinvest-i18n-slices --check` notices:
// the first only types the entry name, the second only diffs the JSON.
//
// A source check rather than a render: what can go wrong is the wiring, and the
// wiring is one line of each page.
import { readdirSync, readFileSync } from "node:fs";
import { join, relative, sep } from "node:path";

import slices from "../i18n-slices.json" with { type: "json" };

const ROOT = join(import.meta.dirname, "..");
const APP = join(ROOT, "app");
const LAYOUT = "app/[locale]/layout.tsx";

const pages = (readdirSync(APP, { recursive: true, encoding: "utf8" }) as string[])
  .filter((file) => file.split(sep).at(-1) === "page.tsx")
  .map((file) => relative(ROOT, join(APP, file)).split(sep).join("/"))
  .sort();

const routes: Readonly<Record<string, readonly string[]>> = slices.routes;
const failures: string[] = [];

for (const page of pages) {
  const source = readFileSync(join(ROOT, page), "utf8");
  if (!(page in routes)) failures.push(`${page}: no entry in i18n-slices.json — run evinvest-i18n-slices`);
  if (!source.includes(`import { I18nScope } from "@evinvest/i18n/react";`))
    failures.push(`${page}: does not import I18nScope from @evinvest/i18n/react`);
  // The entry is the page's own path: a copied page that kept its neighbour's
  // entry type-checks, and carries the neighbour's keys instead of its own.
  if (!source.includes(`<I18nScope messages={routeMessages(locale, ${JSON.stringify(page)})}>`))
    failures.push(`${page}: does not wrap its tree in <I18nScope messages={routeMessages(locale, "${page}")}>`);
  if (!source.includes("</I18nScope>")) failures.push(`${page}: <I18nScope> is never closed around the tree`);
}

for (const route of Object.keys(routes))
  if (!pages.includes(route)) failures.push(`${route}: in i18n-slices.json but no such page — run evinvest-i18n-slices`);

if (!readFileSync(join(ROOT, LAYOUT), "utf8").includes("messages={shellMessages(locale)}"))
  failures.push(`${LAYOUT}: the root I18nProvider is not given shellMessages(locale)`);

if (failures.length > 0) {
  for (const failure of failures) console.error(`✗ ${failure}`);
  process.exit(1);
}
console.log(`i18n scopes: ${pages.length} pages, each wrapped in its own <I18nScope>`);
