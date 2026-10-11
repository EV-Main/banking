"use client";

import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { cn } from "@/shared/lib/cn";

import { chipTranslator } from "../lib/chip-translator";
import type { MenuGroup } from "../lib/host-attributes";

// The chip's controls are hand-written so the bundle stays free of the uikit Button, which
// means the keyboard focus ring has to be written out too.
export const CHIP_FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-ring";

export type MenuIdentity = { label: string; email: string | null; initials: string };

export type MenuLinks = {
  /** The name/email header of the panel; null renders it as plain text. */
  profile: string | null;
  manage: string | null;
  switchAccount: string | null;
};

export type AccountMenuProps = {
  className?: string;
  /** Null is the first-visit skeleton: same box, same width, nothing to open. */
  identity: MenuIdentity | null;
  links: MenuLinks;
  groups: readonly MenuGroup[];
  onSwitchAccount?: () => void;
  onSignOut: () => void;
};

/**
 * Avatar + name + chevron, and the panel it discloses: identity, Manage account, the
 * service groups, Switch account, Sign out. Presentational — it fetches nothing and knows
 * no session; both wrappers (`AccountChip`, `HostAccountChip`) feed it.
 *
 * The skeleton is this same component with no identity, so its box is the final chip's box
 * (178 × 42 px with the w-24 name bar) and nothing moves when the name lands.
 *
 * A disclosure, not an ARIA menu: its entries are plain links Tab already walks, and Escape
 * or a click elsewhere closes it.
 */
export function AccountMenu({ className, identity, links, groups, onSwitchAccount, onSignOut }: AccountMenuProps) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const expanded = open && identity !== null;
  useEffect(() => {
    if (!expanded) return;
    const outside = (e: PointerEvent) => {
      if (e.target instanceof Node && !root.current?.contains(e.target)) setOpen(false);
    };
    const escape = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setOpen(false);
      trigger.current?.focus();
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [expanded]);
  return (
    <div ref={root} className={cn("relative", className)}>
      <button
        ref={trigger}
        type="button"
        disabled={identity === null}
        aria-busy={identity === null || undefined}
        aria-expanded={expanded}
        aria-controls="ev-account-menu"
        onClick={() => setOpen((o) => !o)}
        className={cn(
          "flex min-w-0 cursor-pointer items-center gap-2.5 rounded-lg border-0 bg-transparent px-1.5 py-1 transition-colors hover:bg-ink/5 disabled:cursor-default disabled:hover:bg-transparent",
          CHIP_FOCUS,
        )}
      >
        <span
          className={cn(
            "flex size-8.5 shrink-0 items-center justify-center rounded-full bg-primary-ink/15 text-xs font-semibold text-primary-ink",
            !identity && "animate-pulse",
          )}
        >
          {identity?.initials}
        </span>
        {identity ? (
          <span className="truncate text-sm font-semibold text-ink">{identity.label}</span>
        ) : (
          <span className="block h-3 w-24 animate-pulse rounded bg-ink/10" aria-hidden />
        )}
        <ChevronDown className="size-4 shrink-0 text-ink-soft" aria-hidden />
      </button>
      {expanded && (
        <MenuPanel identity={identity} links={links} groups={groups} onSwitchAccount={onSwitchAccount} onSignOut={onSignOut} />
      )}
    </div>
  );
}

// Spelled out to the reset: the host page's base styles are not this bundle's to rely on.
const ENTRY = cn(
  "block w-full rounded-md bg-transparent px-2 py-1.5 text-left text-sm text-ink no-underline transition-colors hover:bg-ink/5",
  CHIP_FOCUS,
);
const RULE = "my-1 border-0 border-t border-solid border-border";

function MenuPanel({ identity, links, groups, onSwitchAccount, onSignOut }: Omit<AccountMenuProps, "className" | "identity"> & { identity: MenuIdentity }) {
  const t = chipTranslator();
  const header = (
    <>
      <span className="truncate font-semibold">{identity.label}</span>
      {identity.email && <span className="truncate text-ink-soft">{identity.email}</span>}
    </>
  );
  return (
    <div
      id="ev-account-menu"
      className="absolute right-0 top-full z-50 mt-2 flex w-64 flex-col rounded-lg border border-border bg-popover p-1 shadow-md"
    >
      {links.profile ? (
        <a href={links.profile} className={cn(ENTRY, "flex flex-col")}>
          {header}
        </a>
      ) : (
        <div className="flex flex-col px-2 py-1.5 text-sm text-ink">{header}</div>
      )}
      {links.manage && (
        <a href={links.manage} className={ENTRY}>
          {t("auth.manageAccount", "Manage account")}
        </a>
      )}
      {groups.map((group) => (
        <div key={group.id} className="flex flex-col">
          <hr className={RULE} />
          {group.label && <p className="m-0 px-2 py-1 text-xs text-ink-soft">{group.label}</p>}
          {group.items.map((item) => (
            <a
              key={item.id}
              href={item.href}
              aria-current={item.current ? "page" : undefined}
              target={item.external ? "_blank" : undefined}
              rel={item.external ? "noopener noreferrer" : undefined}
              className={cn(ENTRY, item.current && "bg-ink/5 font-semibold")}
            >
              {item.label}
            </a>
          ))}
        </div>
      ))}
      <hr className={RULE} />
      {links.switchAccount && (
        <a href={links.switchAccount} onClick={onSwitchAccount} className={ENTRY}>
          {t("auth.switchAccount", "Switch account")}
        </a>
      )}
      <button type="button" onClick={onSignOut} className={cn(ENTRY, "cursor-pointer border-0")}>
        {t("auth.signOut", "Sign out")}
      </button>
    </div>
  );
}
