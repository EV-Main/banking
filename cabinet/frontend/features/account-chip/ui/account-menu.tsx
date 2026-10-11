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
  links: MenuLinks;
  groups: readonly MenuGroup[];
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
export function AccountMenu({ className, identity, links, groups, onSwitchAccount, onSignOut }: AccountMenuProps) {
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const panelId = useId();
  const { phase, open, close, closeNow } = useDisclosure(panel);
  const expanded = identity !== null && (phase === "open" || phase === "entering");

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
    if (expanded) return close();
    // detail 0 is Enter/Space: the keyboard user lands on the first entry, a pointer does not.
    if (e.detail === 0) openAndFocus("first");
    else open();
  };
  const onTriggerKey = (e: KeyboardEvent<HTMLButtonElement>) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
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
        aria-busy={identity === null || undefined}
        aria-label={identity ? undefined : chipTranslator()("ui.account", "Account")}
        aria-expanded={expanded}
        aria-controls={panelId}
        onClick={onTriggerClick}
        onKeyDown={onTriggerKey}
        className={cn(
          "group flex min-w-0 items-center gap-2.5 rounded-lg border-0 px-1.5 py-1",
          CHIP_PRESS,
          "disabled:cursor-default disabled:hover:bg-transparent disabled:active:bg-transparent",
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
          // sr-only rather than hidden: on a phone the name is still the button's name.
          <span className="truncate text-sm font-semibold text-ink max-sm:sr-only">{identity.label}</span>
        ) : (
          <span className="h-3 w-24 animate-pulse rounded bg-ink/10 max-sm:hidden" aria-hidden />
        )}
        <ChevronDown
          // Duration and easing are motion-safe too: under reduce the computed transition
          // property falls back to `all`, so a bare duration would still animate the turn.
          className="size-4 shrink-0 text-ink-soft group-aria-expanded:rotate-180 motion-safe:transition-transform motion-safe:duration-(--ev-dur-fast) motion-safe:ease-(--ev-ease-out)"
          aria-hidden
        />
      </button>
      {identity && (
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
