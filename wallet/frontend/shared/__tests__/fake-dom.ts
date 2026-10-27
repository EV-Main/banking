// A stand-in document for rendering components with react-dom's client renderer under
// `node --test`. No DOM library is installed; this carries what React reads and writes when
// it creates, updates and removes host nodes and dispatches events at the root, plus what the
// components touch (`focus`, `contains`, `getBoundingClientRect`, `toggleAttribute`, the
// `querySelectorAll`/`closest` shapes they use, an anchor's resolved `href` and `target`, and
// `instanceof Element`/`HTMLAnchorElement`). Anything else throws or is absent, so a test that
// starts relying on more DOM fails loudly instead of passing on a missing feature.
//
// Events do not travel the tree: React listens on the root container, so `click`/`keyDown`
// fire there with `target` set to the node, which is what React resolves the handler from.

const HTML_NS = "http://www.w3.org/1999/xhtml";

export class FakeNode extends EventTarget {
  parentNode: FakeNode | null = null;
  childNodes: FakeNode[] = [];
  readonly nodeType: number;
  readonly nodeName: string;
  readonly ownerDocument: FakeDocument | null;

  // Plain fields, not parameter properties: Node's type stripping cannot emit those.
  constructor(nodeType: number, nodeName: string, ownerDocument: FakeDocument | null) {
    super();
    this.nodeType = nodeType;
    this.nodeName = nodeName;
    this.ownerDocument = ownerDocument;
  }

  get firstChild(): FakeNode | null {
    return this.childNodes[0] ?? null;
  }

  get lastChild(): FakeNode | null {
    return this.childNodes.at(-1) ?? null;
  }

  get nextSibling(): FakeNode | null {
    const siblings = this.parentNode?.childNodes ?? [];
    return siblings[siblings.indexOf(this) + 1] ?? null;
  }

  appendChild<T extends FakeNode>(child: T): T {
    child.parentNode?.removeChild(child);
    this.childNodes.push(child);
    child.parentNode = this;
    return child;
  }

  insertBefore<T extends FakeNode>(child: T, before: FakeNode | null): T {
    if (!before) return this.appendChild(child);
    child.parentNode?.removeChild(child);
    this.childNodes.splice(this.childNodes.indexOf(before), 0, child);
    child.parentNode = this;
    return child;
  }

  removeChild<T extends FakeNode>(child: T): T {
    const at = this.childNodes.indexOf(child);
    if (at < 0) throw new Error("removeChild: not a child");
    this.childNodes.splice(at, 1);
    child.parentNode = null;
    return child;
  }

  contains(other: FakeNode | null): boolean {
    for (let node = other; node; node = node.parentNode) if (node === this) return true;
    return false;
  }

  get textContent(): string {
    return this.childNodes.map((child) => child.textContent).join("");
  }

  set textContent(value: string) {
    for (const child of this.childNodes) child.parentNode = null;
    this.childNodes = [];
    if (value) this.appendChild(new FakeText(value, this.ownerDocument));
  }
}

export class FakeText extends FakeNode {
  nodeValue: string;

  constructor(nodeValue: string, ownerDocument: FakeDocument | null) {
    super(3, "#text", ownerDocument);
    this.nodeValue = nodeValue;
  }

  override get textContent(): string {
    return this.nodeValue;
  }

  override set textContent(value: string) {
    this.nodeValue = value;
  }
}

/** `element.style`: React writes plain properties, custom properties via `setProperty`. */
export type FakeStyle = Record<string, unknown> & {
  setProperty(name: string, value: string): void;
  removeProperty(name: string): void;
  getPropertyValue(name: string): string;
};

function fakeStyle(): FakeStyle {
  const style = {
    setProperty(name: string, value: string) {
      Reflect.set(style, name, value);
    },
    removeProperty(name: string) {
      Reflect.deleteProperty(style, name);
    },
    getPropertyValue(name: string): string {
      const value: unknown = Reflect.get(style, name);
      return typeof value === "string" ? value : "";
    },
  };
  return style;
}

export class FakeElement extends FakeNode {
  readonly tagName: string;
  readonly attributes = new Map<string, string>();
  readonly style: FakeStyle = fakeStyle();
  readonly namespaceURI: string;

  constructor(ownerDocument: FakeDocument | null, tagName: string, namespaceURI: string = HTML_NS) {
    super(1, tagName.toUpperCase(), ownerDocument);
    this.tagName = tagName.toUpperCase();
    this.namespaceURI = namespaceURI;
  }

  getAttribute(name: string): string | null {
    return this.attributes.get(name) ?? null;
  }

  setAttribute(name: string, value: unknown): void {
    this.attributes.set(name, String(value));
  }

  removeAttribute(name: string): void {
    this.attributes.delete(name);
  }

  hasAttribute(name: string): boolean {
    return this.attributes.has(name);
  }

  toggleAttribute(name: string, force?: boolean): boolean {
    const on = force ?? !this.hasAttribute(name);
    if (on) this.setAttribute(name, "");
    else this.removeAttribute(name);
    return on;
  }

  get id(): string {
    return this.getAttribute("id") ?? "";
  }

  focus(): void {
    if (this.ownerDocument) this.ownerDocument.activeElement = this;
  }

  blur(): void {
    if (this.ownerDocument?.activeElement === this) this.ownerDocument.activeElement = null;
  }

  getBoundingClientRect(): DOMRect {
    return { x: 0, y: 0, width: 0, height: 0, top: 0, right: 0, bottom: 0, left: 0, toJSON: () => ({}) };
  }

  /**
   * Only the shapes the chip asks for: a comma list of `tag`, `tag[attr]` and
   * `tag:not(:disabled)`. Anything else throws, so a new selector is noticed here.
   */
  querySelectorAll(selector: string): FakeElement[] {
    const matches = selectorMatcher(selector);
    return descendants(this).filter(matches);
  }

  /** This element or its nearest ancestor matching `selector` (same shapes as above). */
  closest(selector: string): FakeElement | null {
    const matches = selectorMatcher(selector);
    if (matches(this)) return this;
    const parent = this.parentNode;
    return parent instanceof FakeElement ? parent.closest(selector) : null;
  }

  /** As the DOM reflects it on `<a>`: the attribute resolved against the page, or "". */
  get href(): string {
    const raw = this.getAttribute("href");
    if (raw === null) return "";
    try {
      return new URL(raw, String(Reflect.get(globalThis, "location")?.href)).href;
    } catch {
      return raw;
    }
  }

  get target(): string {
    return this.getAttribute("target") ?? "";
  }

  /**
   * What a failed assertion prints: the markup, not the object graph — React's fiber keys and
   * the parent/document links would make `util.inspect` walk the whole tree, out of memory.
   */
  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return this.outerHTML;
  }

  /** Markup as a browser would serialise it, enough to search for text and attributes. */
  get outerHTML(): string {
    const attrs = [...this.attributes].map(([k, v]) => ` ${k}="${v}"`).join("");
    const inner = this.childNodes.map((c) => (c instanceof FakeElement ? c.outerHTML : c.textContent)).join("");
    return `<${this.tagName.toLowerCase()}${attrs}>${inner}</${this.tagName.toLowerCase()}>`;
  }
}

export class FakeDocument extends FakeNode {
  activeElement: FakeElement | null = null;
  readonly documentElement: FakeElement;
  readonly body: FakeElement;

  constructor(lang = "en") {
    super(9, "#document", null);
    this.documentElement = new FakeElement(this, "html");
    this.documentElement.setAttribute("lang", lang);
    Object.defineProperty(this.documentElement, "lang", { value: lang });
    this.body = this.documentElement.appendChild(new FakeElement(this, "body"));
    this.appendChild(this.documentElement);
  }

  createElement(tag: string): FakeElement {
    return new FakeElement(this, tag);
  }

  createElementNS(ns: string, tag: string): FakeElement {
    return new FakeElement(this, tag, ns);
  }

  createTextNode(text: string): FakeText {
    return new FakeText(text, this);
  }
}

function selectorMatcher(selector: string): (el: FakeElement) => boolean {
  const matchers = selector.split(",").map((part) => {
    const m = /^\s*([a-z][a-z-]*)(?:\[([a-z-]+)\])?(:not\(:disabled\))?\s*$/.exec(part);
    if (!m) throw new Error(`fake-dom: unsupported selector "${part}"`);
    const [, tag, attr, enabled] = m;
    return (el: FakeElement) =>
      el.tagName === tag?.toUpperCase() && (!attr || el.hasAttribute(attr)) && (!enabled || !el.hasAttribute("disabled"));
  });
  // Not `t` for the matcher: the i18n extractor reads any `t(…)` call as a translation.
  return (el) => matchers.some((matches) => matches(el));
}

/** Every element under `root`, in document order. */
export function descendants(root: FakeNode): FakeElement[] {
  return root.childNodes.flatMap((child) => (child instanceof FakeElement ? [child, ...descendants(child)] : []));
}

const GLOBALS = [
  "window",
  "document",
  "location",
  "Element",
  "HTMLAnchorElement",
  "HTMLIFrameElement",
  "HTMLElement",
  "customElements",
  "getComputedStyle",
  "IS_REACT_ACT_ENVIRONMENT",
] as const;

/** The page the stand-in document is: what `location` reports and what hrefs resolve against. */
export const PAGE_URL = "https://cabinet.example/en/cabinet";

/**
 * Installs a fresh document as the global one, with `window` as globalThis (as in a page).
 * Must run before the bundle is imported: react-dom decides at load whether a DOM exists.
 * Returns the uninstall.
 */
export function installDom(doc: FakeDocument): () => void {
  Reflect.set(globalThis, "IS_REACT_ACT_ENVIRONMENT", true);
  Reflect.set(globalThis, "window", globalThis);
  Reflect.set(globalThis, "document", doc);
  Reflect.set(globalThis, "location", { href: PAGE_URL, origin: new URL(PAGE_URL).origin });
  Reflect.set(globalThis, "Element", FakeElement);
  Reflect.set(
    globalThis,
    "HTMLAnchorElement",
    class {
      static [Symbol.hasInstance](value: unknown): boolean {
        return value instanceof FakeElement && value.tagName === "A";
      }
    },
  );
  Reflect.set(globalThis, "HTMLIFrameElement", class {});
  // The panel's exit fallback reads its transitions; none are declared here.
  Reflect.set(globalThis, "getComputedStyle", () => ({ transitionDuration: "0s", transitionDelay: "0s" }));
  return () => {
    for (const name of GLOBALS) Reflect.deleteProperty(globalThis, name);
  };
}

/** Modifier keys held during a click; all up by default. */
export type Modifiers = Partial<Record<"ctrlKey" | "metaKey" | "shiftKey" | "altKey", boolean>>;

/** A `click` as React sees it at the root; `detail` 0 is Enter/Space, 1 a pointer. */
export function click(container: FakeElement, target: FakeElement, detail: 0 | 1 = 1, modifiers: Modifiers = {}): Event {
  const keys = { ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, ...modifiers };
  return fire(container, target, Object.assign(new Event("click", { bubbles: true, cancelable: true }), { detail, button: 0 }, keys));
}

/** A `keydown` as React sees it at the root. */
export function keyDown(container: FakeElement, target: FakeElement, key: string): Event {
  return fire(container, target, Object.assign(new Event("keydown", { bubbles: true, cancelable: true }), { key }));
}

function fire(container: FakeElement, target: FakeElement, event: Event): Event {
  Object.defineProperty(event, "target", { value: target });
  container.dispatchEvent(event);
  return event;
}
