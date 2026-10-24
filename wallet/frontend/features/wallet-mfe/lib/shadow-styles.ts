// The stylesheet of the in-page action forms, prepared for a shadow root. Three things do not
// carry over from the light-DOM copy of the same sheet:
//
//   · `:root` matches nothing inside a shadow root, so the token and theme rules move to `:host`;
//   · `@property` and `@font-face` apply only from a document sheet, so they move to one;
//   · the root is closed, so the sheet is adopted rather than linked into the host's head.
//
// A base sheet (`./shadow-base.ts`) goes in front of it, so host styles stop at the root.

import { SHADOW_BASE_CSS } from "./shadow-base";

const ROOT = /:root\b/;

const prepared = new Map<string, Promise<CSSStyleSheet | null>>();
const hoisted = new Set<string>();

// A rule that styles something under `:root` is not a token rule; moving it to `:host` would
// paint the host instead.
function declaresOnlyTokens(rule: CSSStyleRule): boolean {
  return Array.from(rule.style).every((property) => property.startsWith("--"));
}

function rewrite(group: CSSStyleSheet | CSSGroupingRule, documentRules: string[]): void {
  for (let i = group.cssRules.length - 1; i >= 0; i--) {
    const rule = group.cssRules[i];
    if (rule instanceof CSSPropertyRule || rule instanceof CSSFontFaceRule) {
      documentRules.push(rule.cssText);
      group.deleteRule(i);
    } else if (rule instanceof CSSStyleRule && ROOT.test(rule.selectorText) && declaresOnlyTokens(rule)) {
      const declarations = rule.style.cssText;
      group.deleteRule(i);
      group.insertRule(`:host{${declarations}}`, i);
    } else if (rule instanceof CSSGroupingRule) {
      rewrite(rule, documentRules);
    }
  }
}

function hoist(rules: readonly string[]): void {
  const fresh = rules.filter((rule) => !hoisted.has(rule));
  if (fresh.length === 0) return;
  const sheet = new CSSStyleSheet();
  for (const rule of fresh) {
    try {
      sheet.insertRule(rule, sheet.cssRules.length);
      hoisted.add(rule);
    } catch {
      // A rule this browser cannot parse at document level stays out; the form still renders.
    }
  }
  document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
}

async function prepare(href: string): Promise<CSSStyleSheet | null> {
  let css: string;
  try {
    const res = await fetch(href);
    if (!res.ok) return null;
    css = await res.text();
  } catch {
    return null;
  }
  const sheet = new CSSStyleSheet();
  try {
    sheet.replaceSync(css);
  } catch {
    return null;
  }
  const documentRules: string[] = [];
  rewrite(sheet, documentRules);
  hoist(documentRules);
  return sheet;
}

let base: CSSStyleSheet | null = null;

/** The reset and the wall against host styles, adopted first so the wallet sheet's layers win. */
export function shadowBaseSheet(): CSSStyleSheet {
  if (base === null) {
    base = new CSSStyleSheet();
    base.replaceSync(SHADOW_BASE_CSS);
  }
  return base;
}

/** The shadow-root copy of `href`, shared by every form on the page; null when it cannot be read. */
export function shadowSheet(href: string): Promise<CSSStyleSheet | null> {
  let sheet = prepared.get(href);
  if (!sheet) {
    sheet = prepare(href);
    prepared.set(href, sheet);
  }
  return sheet;
}
