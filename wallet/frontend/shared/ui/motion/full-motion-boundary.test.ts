// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// Pins the bundle boundary the motion slice draws (see ./features and
// ../motion-panel/index.ts): `motion.*` components carry the library's full feature set —
// layout projection, drag — onto every page they sit on, so only `Panel`, which animates its
// own size with `layout="size"`, may use them, and it lives behind its own entry point. A
// `motion` import anywhere else puts that code back on every route that imports the module.
//
// Checked on the parsed source, not with a regex: an import split over several lines, an
// alias (`motion as M`), a namespace import and a re-export all reach the same code.
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join, relative, sep } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import ts from "typescript";

const FRONTEND_ROOT = fileURLToPath(new URL("../../../", import.meta.url));
const SOURCE_DIRS = ["app", "application", "views", "features", "entities", "shared", "mfe"];
const PANEL_SLICE = ["shared", "ui", "motion-panel"].join(sep) + sep;

/** Entry points that hand out the `motion` component factory with every feature attached. */
const FULL_MOTION_MODULES = new Set(["motion/react", "motion/react-client", "framer-motion"]);
/** Names that bring layout projection and drag with them. */
const FULL_FEATURE_NAMES = new Set(["motion", "domMax"]);

function walk(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path, out);
    else out.push(path);
  }
  return out;
}

function sources(): string[] {
  return SOURCE_DIRS.flatMap((dir) => walk(join(FRONTEND_ROOT, dir))).filter((path) => /\.(tsx?|jsx?|mjs)$/.test(path));
}

/** Every way `source` reaches the full feature set, as "file: what" lines. */
function fullMotionUses(path: string): string[] {
  const file = relative(FRONTEND_ROOT, path);
  const source = ts.createSourceFile(path, readFileSync(path, "utf8"), ts.ScriptTarget.Latest, true);
  const found: string[] = [];
  const visit = (node: ts.Node) => {
    if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier && ts.isStringLiteral(node.moduleSpecifier)) {
      const from = node.moduleSpecifier.text;
      if (FULL_MOTION_MODULES.has(from)) {
        if (ts.isImportDeclaration(node)) {
          const bindings = node.importClause?.namedBindings;
          if (node.importClause?.name) found.push(`${file}: default import from "${from}"`);
          if (bindings && ts.isNamespaceImport(bindings)) found.push(`${file}: import * as ${bindings.name.text} from "${from}"`);
          if (bindings && ts.isNamedImports(bindings)) {
            for (const el of bindings.elements) {
              const imported = (el.propertyName ?? el.name).text;
              if (FULL_FEATURE_NAMES.has(imported)) found.push(`${file}: import { ${imported} } from "${from}"`);
            }
          }
        } else if (!node.exportClause) {
          found.push(`${file}: export * from "${from}"`);
        } else if (ts.isNamedExports(node.exportClause)) {
          for (const el of node.exportClause.elements) {
            const exported = (el.propertyName ?? el.name).text;
            if (FULL_FEATURE_NAMES.has(exported)) found.push(`${file}: export { ${exported} } from "${from}"`);
          }
        } else {
          found.push(`${file}: export * as ${node.exportClause.name.text} from "${from}"`);
        }
      }
    }
    if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword) {
      const [arg] = node.arguments;
      if (arg && ts.isStringLiteral(arg) && FULL_MOTION_MODULES.has(arg.text)) found.push(`${file}: import("${arg.text}")`);
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return found;
}

test("only the motion-panel slice imports the full motion feature set", () => {
  const offenders = sources()
    .filter((path) => !relative(FRONTEND_ROOT, path).startsWith(PANEL_SLICE))
    .flatMap(fullMotionUses);

  assert.deepEqual(
    offenders,
    [],
    "`motion.*` (and `domMax`) load layout projection and drag on every route that imports " +
      "the module. Use an `m` component under `MotionFeatures` from @/shared/ui/motion, or, " +
      "for size animation, `Panel` from @/shared/ui/motion-panel:\n" +
      offenders.join("\n"),
  );
});

test("the motion-panel slice is where Panel gets its motion components", () => {
  // The boundary above is only meaningful while the one legitimate user is where the test
  // expects it: if Panel moved, the allow-list would silently cover nothing.
  const panelUses = walk(join(FRONTEND_ROOT, PANEL_SLICE)).flatMap(fullMotionUses);

  assert.deepEqual(panelUses, ['shared/ui/motion-panel/panel.tsx: import { motion } from "motion/react"']);
});
