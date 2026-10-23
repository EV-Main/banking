// Run with `npm run test` (Node's built-in runner, native type-stripping).
//
// A click on a chip menu link is offered to the host's router only when it is a plain,
// same-origin navigation the browser would do in this tab; everything else (new tab, new
// window, download, another origin, a link already handled) stays the browser's. What is
// pinned here: each refusal on its own, the path handed over (path + query + hash, never the
// origin), and that `dispatchNavigate` reports a take only when a listener cancelled the
// event, which bubbles and crosses shadow roots.
import assert from "node:assert/strict";
import test from "node:test";

import { delegatedHref, dispatchNavigate, type LinkAnchor, type LinkClick } from "./navigate-event.ts";

const PAGE = { href: "https://cabinet.example/en/cabinet", origin: "https://cabinet.example" };
const PLAIN: LinkClick = { button: 0, ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, defaultPrevented: false };

/** An `<a>` as the DOM reports it: `href` resolved, `target` as written, attributes by name. */
function anchor(href: string, attrs: { target?: string; download?: boolean } = {}): LinkAnchor {
  return { href, target: attrs.target ?? "", hasAttribute: (name) => name === "download" && attrs.download === true };
}

const SETTINGS = anchor("https://cabinet.example/en/cabinet/settings?tab=security#top");

// ── delegatedHref: what is offered ──────────────────────────────────────────

test("a plain same-origin click offers path, query and hash, without the origin", () => {
  assert.equal(delegatedHref(PLAIN, SETTINGS, PAGE), "/en/cabinet/settings?tab=security#top");
});

test("a relative href is resolved against the page", () => {
  assert.equal(delegatedHref(PLAIN, anchor("profile?x=1"), PAGE), "/en/profile?x=1");
});

test("target _self is this tab and is still offered", () => {
  assert.equal(delegatedHref(PLAIN, anchor("https://cabinet.example/en/cabinet", { target: "_self" }), PAGE), "/en/cabinet");
});

// ── delegatedHref: what stays the browser's ─────────────────────────────────

test("a middle or right click stays the browser's", () => {
  assert.equal(delegatedHref({ ...PLAIN, button: 1 }, SETTINGS, PAGE), null, "middle");
  assert.equal(delegatedHref({ ...PLAIN, button: 2 }, SETTINGS, PAGE), null, "right");
});

test("a click with any modifier key stays the browser's", () => {
  assert.equal(delegatedHref({ ...PLAIN, ctrlKey: true }, SETTINGS, PAGE), null, "ctrl");
  assert.equal(delegatedHref({ ...PLAIN, metaKey: true }, SETTINGS, PAGE), null, "meta");
  assert.equal(delegatedHref({ ...PLAIN, shiftKey: true }, SETTINGS, PAGE), null, "shift");
  assert.equal(delegatedHref({ ...PLAIN, altKey: true }, SETTINGS, PAGE), null, "alt");
});

test("a link that opens in another browsing context stays the browser's", () => {
  assert.equal(delegatedHref(PLAIN, anchor(SETTINGS.href, { target: "_blank" }), PAGE), null, "_blank");
  assert.equal(delegatedHref(PLAIN, anchor(SETTINGS.href, { target: "docs" }), PAGE), null, "named window");
});

test("a download link stays the browser's", () => {
  assert.equal(delegatedHref(PLAIN, anchor(SETTINGS.href, { download: true }), PAGE), null);
});

test("a click already prevented by someone else is not offered again", () => {
  assert.equal(delegatedHref({ ...PLAIN, defaultPrevented: true }, SETTINGS, PAGE), null);
});

test("another origin stays the browser's, whatever its path", () => {
  assert.equal(delegatedHref(PLAIN, anchor("https://evil.example/en/cabinet"), PAGE), null, "other host");
  assert.equal(delegatedHref(PLAIN, anchor("http://cabinet.example/en/cabinet"), PAGE), null, "other scheme");
  assert.equal(delegatedHref(PLAIN, anchor("https://cabinet.example:8443/en/cabinet"), PAGE), null, "other port");
  assert.equal(delegatedHref(PLAIN, anchor("//evil.example/x"), PAGE), null, "protocol-relative");
});

test("an unparseable href stays the browser's instead of throwing", () => {
  assert.equal(delegatedHref(PLAIN, anchor("http://[::1"), PAGE), null);
});

// ── dispatchNavigate ────────────────────────────────────────────────────────

test("the offer is one bubbling, composed, cancelable ev:navigate carrying the href", () => {
  const source = new EventTarget();
  const seen: Event[] = [];
  source.addEventListener("ev:navigate", (e) => void seen.push(e));

  dispatchNavigate(source, "/en/cabinet/profile");

  assert.equal(seen.length, 1);
  const [event] = seen;
  assert.ok(event instanceof CustomEvent);
  assert.deepEqual(event.detail, { href: "/en/cabinet/profile" });
  assert.deepEqual([event.bubbles, event.composed, event.cancelable], [true, true, true]);
});

test("the host took it only when a listener called preventDefault", () => {
  const taken = new EventTarget();
  taken.addEventListener("ev:navigate", (e) => e.preventDefault());

  assert.equal(dispatchNavigate(taken, "/en/cabinet"), true);
});

test("an offer nobody cancels — no listener, or one that only looks — is not taken", () => {
  const looked = new EventTarget();
  looked.addEventListener("ev:navigate", () => undefined);

  assert.equal(dispatchNavigate(new EventTarget(), "/en/cabinet"), false, "no listener");
  assert.equal(dispatchNavigate(looked, "/en/cabinet"), false, "a listener that does not cancel");
});
