// The chip's stylesheet: attach it once, and know when it applies.
//
// The chip must not paint before its CSS: in 15 of 15 measured loads the first frame was
// unstyled — avatar collapsed to 0×0, then a jump to the real box. Waiting needs care
// because the sheet does not stay where this bundle put it: site_conductor's
// `containRemoteStyles` swaps our `<link>` for `<style>@import url(…) layer(reamfe)</style>`
// to demote it below the host's layers, and a detached link never fires `load`. So
// readiness is read off `document.styleSheets` — the sheet itself, or an `@import` of it
// whose rules have arrived — and re-checked on every `load`/`error` in the document, which
// both a link and an importing `<style>` fire once their CSS has landed.

/** Never hidden longer than this: a lost load event must not cost the host its account chip. */
const READY_TIMEOUT_MS = 4000;

export function stylesheetHref(): string {
  const src = import.meta.url;
  // String math, not `new URL(…, import.meta.url)`: esbuild would try to resolve and inline
  // the sheet, which the Tailwind CLI emits separately.
  return src.slice(0, src.lastIndexOf("/") + 1) + "account-chip.css";
}

/** Whether `node` is our sheet: a link to it, or a style importing it (the conductor's demotion). */
function mentions(node: EventTarget | null, href: string): boolean {
  if (node instanceof HTMLLinkElement) return node.href === href;
  if (node instanceof HTMLStyleElement) return node.textContent?.includes(href) ?? false;
  return false;
}

/**
 * Append the `<link>` unless the server rendered one or the host has already demoted it.
 * Returns the href to wait on: a server-rendered `link[data-mfe]` wins, since its URL may be
 * spelled differently from the one derived from this bundle's location.
 */
export function ensureStylesheet(tag: string, href: string): string {
  for (const node of Array.from(document.head.querySelectorAll("link, style"))) {
    if (node instanceof HTMLLinkElement && node.dataset.mfe === tag) return node.href;
    if (mentions(node, href)) return href;
  }
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = href;
  link.dataset.mfe = tag;
  document.head.appendChild(link);
  return href;
}

function loaded(sheet: CSSStyleSheet): boolean {
  try {
    return sheet.cssRules.length > 0;
  } catch {
    // Cross-origin rules are unreadable only once the sheet exists, i.e. has loaded.
    return true;
  }
}

/** True once our rules are in the document, linked directly or imported. */
export function stylesheetReady(href: string): boolean {
  for (const sheet of Array.from(document.styleSheets)) {
    if (sheet.href === href) return loaded(sheet);
    let rules: CSSRuleList;
    try {
      rules = sheet.cssRules;
    } catch {
      continue;
    }
    // `@import` may only follow `@charset` and `@layer` statements, so the scan stops at the
    // first other rule instead of walking a host stylesheet's thousands.
    for (const rule of Array.from(rules)) {
      if (rule instanceof CSSImportRule) {
        if (new URL(rule.href, sheet.href ?? document.baseURI).href === href && rule.styleSheet && loaded(rule.styleSheet)) return true;
      } else if (!(rule instanceof CSSLayerStatementRule)) {
        break;
      }
    }
  }
  return false;
}

let ready: Promise<void> | null = null;

/** Resolves when the sheet applies, or fails, or READY_TIMEOUT_MS passes. Shared by every instance. */
export function whenStylesheetReady(href: string): Promise<void> {
  ready ??= stylesheetReady(href)
    ? Promise.resolve()
    : new Promise<void>((resolve) => {
        const done = () => {
          document.removeEventListener("load", check, true);
          document.removeEventListener("error", fail, true);
          clearTimeout(timer);
          resolve();
        };
        // Capture phase: `load` and `error` do not bubble, but every element's pass through document.
        const check = () => {
          if (stylesheetReady(href)) done();
        };
        const fail = (e: Event) => {
          if (mentions(e.target, href)) done();
        };
        document.addEventListener("load", check, true);
        document.addEventListener("error", fail, true);
        const timer = setTimeout(done, READY_TIMEOUT_MS);
      });
  return ready;
}
