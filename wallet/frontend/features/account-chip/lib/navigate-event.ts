// Link clicks in the chip's menu, offered to the host's router before the browser follows them.
//
// The chip renders in its own React root inside a custom element, outside the host's tree, so
// it has no router to push to and its entries are plain `<a href>`s: every one was a full page
// load, where a link in the host's own UI is a client-side transition. So the chip asks: it
// dispatches a cancelable `ev:navigate` from the element, and a host that routes the path
// itself calls `preventDefault()` and pushes it; the chip then cancels the click. A host that
// does not listen (or does not want that path) leaves the event alone and the browser
// navigates as it always has — the contract is additive, apiVersion stays 2.
//
// Pure; relative imports, no `@/`, so `node --test` runs it as is.

import { NAVIGATE_EVENT, type NavigateDetail } from "../../../shared/config/chip-element.ts";

/** The parts of a click this decision reads (a DOM `MouseEvent` or React's wrapper). */
export type LinkClick = {
  button: number;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  defaultPrevented: boolean;
};

/** The parts of the clicked `<a>` this decision reads. */
export type LinkAnchor = {
  href: string;
  target: string;
  hasAttribute(name: string): boolean;
};

/**
 * The path to offer the host, or null when the click is the browser's to keep: a modified or
 * non-primary click (new tab, new window, download), a link that opens elsewhere (`target`,
 * `download`), one already handled, or another origin, which no host router can reach.
 * Enter on a focused link arrives as the same click with `button` 0, so the keyboard needs
 * nothing of its own.
 */
export function delegatedHref(click: LinkClick, anchor: LinkAnchor, page: { href: string; origin: string }): string | null {
  if (click.defaultPrevented || click.button !== 0) return null;
  if (click.ctrlKey || click.metaKey || click.shiftKey || click.altKey) return null;
  if ((anchor.target && anchor.target !== "_self") || anchor.hasAttribute("download")) return null;
  let url: URL;
  try {
    url = new URL(anchor.href, page.href);
  } catch {
    return null;
  }
  if (url.origin !== page.origin) return null;
  return url.pathname + url.search + url.hash;
}

/** Offer `href` to the host from `source`; true when the host took it (`preventDefault`). */
export function dispatchNavigate(source: EventTarget, href: string): boolean {
  const detail: NavigateDetail = { href };
  // composed: a host that wraps the element in its own shadow root still hears it.
  const event = new CustomEvent<NavigateDetail>(NAVIGATE_EVENT, { bubbles: true, composed: true, cancelable: true, detail });
  return !source.dispatchEvent(event);
}
