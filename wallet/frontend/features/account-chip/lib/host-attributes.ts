// What a host page tells the chip in host mode (`data-mode="host"`), read off the custom
// element's attributes. Host mode exists for apps on another origin with their own session
// (the Service-Arb panel): the chip cannot ask the wallet host who is signed in there, so the
// host says it, and the chip fetches nothing.
//
// Every attribute is outside input — a host on another release, a typo, a stale template —
// so each one is parsed here, and anything that does not parse reads as absent rather than
// half-rendering. Pure and import-free so `node --test` runs it as is.

export type MenuItem = {
  id: string;
  href: string;
  label: string;
  /** Opens in a new tab. */
  external: boolean;
  /** The service this page belongs to: `aria-current="page"` and highlighted. */
  current: boolean;
};

export type MenuGroup = {
  id: string;
  /** Heading above the group, already localised by the host; none, no heading. */
  label: string | null;
  items: MenuItem[];
};

export type HostChipProps = {
  name: string | null;
  email: string | null;
  manageHref: string | null;
  profileHref: string | null;
  switchHref: string | null;
  groups: MenuGroup[];
};

/** The element's attribute names. Frozen with the contract; a rename is apiVersion 3. */
export const HOST_ATTRS = {
  mode: "data-mode",
  name: "data-name",
  email: "data-email",
  manageHref: "data-manage-href",
  profileHref: "data-profile-href",
  switchHref: "data-switch-href",
  groups: "data-groups",
  intent: "data-intent",
  returnTo: "data-return-to",
  /** Either mode; `menu` keeps the name below `sm` (a drawer row). Read by CSS only. */
  layout: "data-layout",
} as const;

// Bounds on what a header menu can sensibly hold, so a runaway attribute cannot lay out a
// thousand rows on every page of the host.
const MAX_GROUPS = 8;
const MAX_ITEMS = 16;
// The longest valid email address; no name or label legitimately runs longer.
const MAX_TEXT = 254;

function text(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  const value = raw.trim();
  return value && value.length <= MAX_TEXT ? value : null;
}

/**
 * An href the chip will render: root-relative or absolute http(s). Everything else —
 * `javascript:`, `data:`, protocol-relative `//evil`, control characters, backslashes, bare
 * words — is refused, because the value lands in an `<a href>` on a page that holds a session.
 */
export function safeHref(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  // Browsers strip tabs and newlines from URLs and read a backslash as `/`, so `/<TAB>/evil`
  // or `/\evil` parse as protocol-relative; refused wherever they sit, before any trimming.
  if (/[\u0000-\u001F\u007F\\]/.test(raw)) return null;
  const value = raw.trim();
  if (!value || value.length > 2048) return null;
  if (value.startsWith("/")) {
    if (value.startsWith("//")) return null;
    // Belt and braces: whatever the parser makes of it must stay on the page's own origin.
    try {
      return new URL(value, "https://h.invalid").origin === "https://h.invalid" ? value : null;
    } catch {
      return null;
    }
  }
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:" ? value : null;
  } catch {
    return null;
  }
}

function parseItem(raw: unknown): MenuItem | null {
  if (typeof raw !== "object" || raw === null) return null;
  const o = raw as Record<string, unknown>;
  const id = text(o.id);
  const href = safeHref(o.href);
  const label = text(o.label);
  if (!id || !href || !label) return null;
  if (o.external !== undefined && typeof o.external !== "boolean") return null;
  if (o.current !== undefined && typeof o.current !== "boolean") return null;
  return { id, href, label, external: o.external === true, current: o.current === true };
}

function parseGroup(raw: unknown): MenuGroup | null {
  if (typeof raw !== "object" || raw === null) return null;
  const o = raw as Record<string, unknown>;
  const id = text(o.id);
  if (!id) return null;
  const label = o.label === undefined ? null : text(o.label);
  if (o.label !== undefined && label === null) return null;
  if (!Array.isArray(o.items) || o.items.length === 0 || o.items.length > MAX_ITEMS) return null;
  const items: MenuItem[] = [];
  for (const entry of o.items) {
    const item = parseItem(entry);
    if (!item) return null;
    items.push(item);
  }
  return { id, label, items };
}

/**
 * `data-groups`: JSON `[{ id, label?, items: [{ id, href, label, external?, current? }] }]`.
 * All or nothing — one malformed entry empties the list, so a host bug shows up as a missing
 * section rather than as a menu with holes in it.
 */
export function parseGroups(raw: string | null): MenuGroup[] {
  if (!raw) return [];
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(value) || value.length > MAX_GROUPS) return [];
  const groups: MenuGroup[] = [];
  for (const entry of value) {
    const group = parseGroup(entry);
    if (!group) return [];
    groups.push(group);
  }
  return groups;
}

export function isHostMode(read: (name: string) => string | null): boolean {
  return read(HOST_ATTRS.mode) === "host";
}

/** Everything host mode renders, from an attribute reader (`el.getAttribute.bind(el)`). */
export function readHostProps(read: (name: string) => string | null): HostChipProps {
  return {
    name: text(read(HOST_ATTRS.name)),
    email: text(read(HOST_ATTRS.email)),
    manageHref: safeHref(read(HOST_ATTRS.manageHref)),
    profileHref: safeHref(read(HOST_ATTRS.profileHref)),
    switchHref: safeHref(read(HOST_ATTRS.switchHref)),
    groups: parseGroups(read(HOST_ATTRS.groups)),
  };
}
