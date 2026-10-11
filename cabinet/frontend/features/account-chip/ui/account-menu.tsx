"use client";

import { ChevronDown } from "lucide-react";
import { useEffect, useId, useRef, type KeyboardEvent, type MouseEvent } from "react";

import { cn } from "@/shared/lib/cn";

import { chipTranslator } from "../lib/chip-translator";
import type { MenuGroup } from "../lib/host-attributes";
import { CHIP_FOCUS, CHIP_PRESS, type MenuIdentity, type MenuLinks } from "./chip-parts";
import { MenuPanel } from "./menu-panel";
import { useDisclosure } from "./use-disclosure";

export { CHIP_FOCUS, type MenuIdentity, type MenuLinks } from "./chip-parts";

export type AccountMenuProps = {
  className?: string;
  /** Null is the first-visit skeleton: same box, same width, nothing to open. */
  identity: MenuIdentity | null;
  /**
   * An identity painted before the session confirmed it (the last visit's cache): shown, but
   * nothing opens — on a shared machine the panel would be the previous person's account.
   * The caller switches it by remounting (AccountChip renders a locked and a live chip as
   * different trees), so an open phase never survives into a lock or out of one.
   */
  locked?: boolean;
  links: MenuLinks;
  groups: readonly MenuGroup[];
  /** An action from the panel failed after it closed (sign out); shown under the chip. */
  error?: string | null;
  onSwitchAccount?: () => void;
  onSignOut: () => void;
};

const ENTRIES = "a[href], button:not(:disabled)";

/**
 * Avatar + name + chevron, and the panel it discloses: identity, Manage account, the
 * service groups, Switch account, Sign out. Presentational — it fetches nothing and knows
 * no session; both wrappers (`AccountChip`, `HostAccountChip`) feed it.
 *
 * The skeleton is this same component with no identity, so its box is the final chip's box
 * (178 × 42 px with the w-24 name bar) and nothing moves when the name lands. Below `sm`
 * the name and the bar step out (avatar + chevron); the host's slot keeps its own reserve.
 *
 * A disclosure, not an ARIA menu: the entries stay plain links and buttons. Arrow keys,
 * Home and End are a shortcut on top of Tab, not a replacement for it; Escape, Tab out and
 * a click elsewhere close it.
 */
export function AccountMenu({ className, identity, locked = false, links, groups, error = null, onSwitchAccount, onSignOut }: AccountMenuProps) {
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const panelId = useId();
  const { phase, open, close, closeNow } = useDisclosure(panel);
  const openable = identity !== null && !locked;
  const expanded = openable && (phase === "open" || phase === "entering");

  useEffect(() => {
    if (!expanded) return;
    const outside = (e: PointerEvent) => {
      if (e.target instanceof Node && !root.current?.contains(e.target)) close();
    };
    const escape = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "Escape") return;
      close();
      // Back to the trigger only when focus was ours to lose: Escape typed into some other
      // field on the host page must not yank the caret into the header.
      const active = document.activeElement;
      if (active === document.body || (active && root.current?.contains(active))) trigger.current?.focus();
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [expanded, close]);

  const entries = () => [...(panel.current?.querySelectorAll<HTMLElement>(ENTRIES) ?? [])];
  const openAndFocus = (edge: "first" | "last") => {
    open();
    const list = entries();
    (edge === "first" ? list[0] : list.at(-1))?.focus();
  };

  const onTriggerClick = (e: MouseEvent<HTMLButtonElement>) => {
    if (!openable) return;
    if (expanded) return close();
    // detail 0 is Enter/Space: the keyboard user lands on the first entry, a pointer does not.
    if (e.detail === 0) openAndFocus("first");
    else open();
  };
  const onTriggerKey = (e: KeyboardEvent<HTMLButtonElement>) => {
    if (!openable || (e.key !== "ArrowDown" && e.key !== "ArrowUp")) return;
    e.preventDefault();
    openAndFocus(e.key === "ArrowDown" ? "first" : "last");
  };
  const onPanelKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Tab") return closeNow();
    const list = entries();
    const at = list.findIndex((el) => el === document.activeElement);
    const next =
      e.key === "ArrowDown" ? list[(at + 1) % list.length]
      : e.key === "ArrowUp" ? list[(at - 1 + list.length) % list.length]
      : e.key === "Home" ? list[0]
      : e.key === "End" ? list.at(-1)
      : undefined;
    if (!next) return;
    e.preventDefault();
    next.focus();
  };

  return (
    <div ref={root} className={cn("relative", className)}>
      <button
        ref={trigger}
        type="button"
        disabled={identity === null}
        // aria-disabled rather than `disabled` while locked: the cached name stays reachable
        // with Tab and is announced as unavailable, instead of vanishing from the tab order.
        aria-disabled={locked || undefined}
        aria-busy={identity === null || locked || undefined}
        aria-label={identity ? undefined : chipTranslator()("ui.account", "Account")}
        aria-expanded={expanded}
        aria-controls={panelId}
        onClick={onTriggerClick}
        onKeyDown={onTriggerKey}
        className={cn(
          "group flex min-w-0 items-center gap-2.5 rounded-lg border-0 px-1.5 py-1",
          CHIP_PRESS,
          "disabled:cursor-default disabled:hover:bg-transparent disabled:active:bg-transparent",
          "aria-disabled:cursor-default aria-disabled:hover:bg-transparent aria-disabled:active:bg-transparent",
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
          // sr-only rather than hidden: on a phone the name is still the button's name. A host
          // that gives the chip a full-width row (a mobile drawer) sets `data-layout="menu"` on
          // the element and keeps the name; pure CSS, so no prop threads through both modes.
          <span className="truncate text-sm font-semibold text-ink max-sm:sr-only max-sm:in-data-[layout=menu]:not-sr-only">
            {identity.label}
          </span>
        ) : (
          <span className="h-3 w-24 animate-pulse rounded bg-ink/10 max-sm:hidden max-sm:in-data-[layout=menu]:inline-block" aria-hidden />
        )}
        <ChevronDown
          // Duration and easing are motion-safe too: under reduce the computed transition
          // property falls back to `all`, so a bare duration would still animate the turn.
          className="size-4 shrink-0 text-ink-soft group-aria-expanded:rotate-180 motion-safe:transition-transform motion-safe:duration-(--ev-dur-fast) motion-safe:ease-(--ev-ease-out)"
          aria-hidden
        />
      </button>
      {error && !expanded && (
        // Where the panel was, so the eye is already there; the open panel takes its place
        // again on the retry.
        <p role="alert" className="absolute right-0 top-full z-50 m-0 mt-2 box-border w-64 max-w-[calc(100vw-1rem)] rounded-lg border border-border bg-popover px-3 py-2 text-sm text-ink shadow-md">
          {error}
        </p>
      )}
      {openable && (
        <MenuPanel
          id={panelId}
          panelRef={panel}
          phase={phase}
          identity={identity}
          links={links}
          groups={groups}
          onKeyDown={onPanelKey}
          onPick={close}
          onSwitchAccount={onSwitchAccount}
          onSignOut={onSignOut}
        />
      )}
    </div>
  );
}
