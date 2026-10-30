// The element's stylesheet: attach it once, and know when it applies.
//
// Delivered as an already-filled `<style>`, not a `<link>`. A stylesheet that arrives late
// from the network — any `<link>` or `@import`, even an empty one — makes Chrome rebuild
// the document's font set. On the landing that picks up an `optional`-display web font
// the page had already given up on (Playfair, after first paint fell back to Georgia), and
// the hero headline re-wraps: "China+1" narrows 345 → 304 px, the hero moves 12 px, CLS
// 0.0039–0.0048 on slow 4G in every run. The same CSS inserted inline moved nothing in 3 of
// 3 runs (V1 bisection, measure/bisect/r3-mechanism.json, v-fix2-slow4g.json). The element
// must be appended WITH its text: an empty `<style>` filled afterwards still triggers it.
//
// The CSS goes in as `@layer reamfe { … }`, the layer the conductor demoted the old link
// into. That nests the compiled theme variables inside the layer too; harmless at runtime —
// the reason `mfe.css` keeps its theme import unlayered is the utility generator at build
// time, which this does not touch.
//
// The element must not paint before its CSS (15 of 15 measured loads had an unstyled first
// frame without the wait). A failed fetch falls back to the `<link>`, whose readiness is
// read off `document.styleSheets` because site_conductor's `containRemoteStyles` may swap
// it for `<style>@import url(…) layer(reamfe)</style>`, and a detached link never fires
// `load`.

/** Never hidden longer than this: a lost load event must not cost the host its wallet element. */
const READY_TIMEOUT_MS = 4000;

/** The sibling sheet `file` next to this bundle. The wallet bundle reuses this module with its own sheet. */
export function stylesheetHref(file: string): string {
  const src = import.meta.url;
  // String math, not `new URL(…, import.meta.url)`: esbuild would try to resolve and inline
  // the sheet, which the Tailwind CLI emits separately.
  return src.slice(0, src.lastIndexOf("/") + 1) + file;
}

/** Whether `node` is our sheet: a link to it, or a style importing it (the conductor's demotion). */
function mentions(node: EventTarget | null, href: string): boolean {
  if (node instanceof HTMLLinkElement) return node.href === href;
  if (node instanceof HTMLStyleElement) return node.textContent?.includes(href) ?? false;
  return false;
}

function loaded(sheet: CSSStyleSheet): boolean {
  try {
    return sheet.cssRules.length > 0;
  } catch {
    // Cross-origin rules are unreadable only once the sheet exists, i.e. has loaded.
    return true;
  }
}

/** True once a linked copy of our rules is in the document, directly or imported. */
function linkReady(href: string): boolean {
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

/** Resolves once a linked sheet applies or fails to; the caller caps the wait. */
function whenLinkReady(href: string): Promise<void> {
  if (linkReady(href)) return Promise.resolve();
  return new Promise<void>((resolve) => {
    const done = () => {
      document.removeEventListener("load", check, true);
      document.removeEventListener("error", fail, true);
      resolve();
    };
    // Capture phase: `load` and `error` do not bubble, but every element's pass through document.
    const check = () => {
      if (linkReady(href)) done();
    };
    const fail = (e: Event) => {
      if (mentions(e.target, href)) done();
    };
    document.addEventListener("load", check, true);
    document.addEventListener("error", fail, true);
  });
}

function appendLink(tag: string, href: string): Promise<void> {
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = href;
  link.dataset.mfe = tag;
  document.head.appendChild(link);
  return whenLinkReady(href);
}

async function inlineOrLink(tag: string, href: string): Promise<void> {
  let css: string;
  try {
    // Defaults on purpose: the bundle and its sheet share an origin on every host (the
    // conductor serves /cabinet/mfe, the panel proxies it), so this is a same-origin read
    // under the `/mfe/*` Cache-Control in next.config.ts; a cross-origin host without CORS lands in the fallback.
    const res = await fetch(href);
    if (!res.ok) throw new Error(`stylesheet: HTTP ${res.status}`);
    css = await res.text();
  } catch {
    return appendLink(tag, href);
  }
  // A second copy of this bundle (or an older one with its `<link>`) may have won meanwhile.
  const found = existing(tag, href);
  if (found) return found.kind === "linked" ? whenLinkReady(found.href) : undefined;
  const style = document.createElement("style");
  style.dataset.mfe = tag;
  // Filled before it is attached — see the note at the top.
  style.textContent = `@layer reamfe{${css}}`;
  document.head.appendChild(style);
  // A host whose CSP `style-src` lacks 'unsafe-inline' (as `walletCsp()` of @evinvest/wallet
  // builds it) blocks the inline copy, and a blocked `<style>` gets no sheet. Without this the
  // element renders unstyled; the `<link>` is allowed by the bundle directory source.
  if (style.sheet === null) {
    style.remove();
    return appendLink(tag, href);
  }
}

/**
 * What is already in the head: our inline style (ready the moment it exists), a link a
 * server rendered or an older bundle added — with the href to wait on, since a server's
 * spelling of the URL may differ from this bundle's — or nothing.
 */
function existing(tag: string, href: string): { kind: "inline" } | { kind: "linked"; href: string } | null {
  for (const node of Array.from(document.head.querySelectorAll("link, style"))) {
    if (node instanceof HTMLStyleElement && node.dataset.mfe === tag) return { kind: "inline" };
    if (node instanceof HTMLLinkElement && node.dataset.mfe === tag) return { kind: "linked", href: node.href };
    if (mentions(node, href)) return { kind: "linked", href };
  }
  return null;
}

let ready: Promise<void> | null = null;

/**
 * Attaches the sheet unless it is already there, and resolves when it applies, fails, or
 * READY_TIMEOUT_MS passes. Shared by every instance on the page.
 */
export function whenStylesheetReady(tag: string, href: string): Promise<void> {
  ready ??= new Promise<void>((resolve) => {
    const timer = setTimeout(resolve, READY_TIMEOUT_MS);
    const found = existing(tag, href);
    const attached = found === null ? inlineOrLink(tag, href) : found.kind === "inline" ? Promise.resolve() : whenLinkReady(found.href);
    void attached.then(() => {
      clearTimeout(timer);
      resolve();
    });
  });
  return ready;
}
