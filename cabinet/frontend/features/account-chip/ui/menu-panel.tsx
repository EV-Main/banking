"use client";

import { ChevronRight } from "lucide-react";
import { Fragment, type KeyboardEvent, type Ref } from "react";

import { cn } from "@/shared/lib/cn";

import { chipTranslator } from "../lib/chip-translator";
import type { MenuGroup } from "../lib/host-attributes";
import { CHIP_FOCUS, CHIP_PRESS, type MenuIdentity, type MenuLinks } from "./chip-parts";
import type { DisclosurePhase } from "./use-disclosure";

// Spelled out to the reset: the host page's base styles are not this bundle's to rely on.
// 44 px rows are the touch-target floor; `flex` rather than `block` so the row can hold it,
// and `box-border` because a host without preflight would add the padding on top of it.
const ENTRY = cn(
  "box-border flex min-h-11 w-full items-center gap-2 rounded-md border-0 px-2 py-1.5 text-left text-sm text-ink no-underline",
  CHIP_PRESS,
  CHIP_FOCUS,
);
const RULE = "my-1 border-0 border-t border-solid border-border";

// Open is the default look and closed the variant, never the reverse: a host whose own
// `utilities` layer has the zero-opacity or 97 % scale utility would outrank an open-state variant of
// ours and leave an open panel invisible but clickable (see chip-parts.ts). Under reduced
// motion only opacity moves; the panel still fades so the state change is not a cut.
const PANEL = cn(
  "absolute right-0 top-full z-50 mt-2 flex w-64 max-w-[calc(100vw-1rem)] origin-top-right flex-col rounded-lg border border-border bg-popover p-1 shadow-md",
  "transition-[opacity,translate,scale] duration-(--ev-dur-fast) ease-(--ev-ease-out) motion-reduce:transition-opacity",
  "data-[state=closed]:opacity-0 motion-safe:data-[state=closed]:-translate-y-1 motion-safe:data-[state=closed]:scale-97",
);

export type MenuPanelProps = {
  id: string;
  panelRef: Ref<HTMLDivElement>;
  phase: DisclosurePhase;
  identity: MenuIdentity;
  links: MenuLinks;
  groups: readonly MenuGroup[];
  onKeyDown: (e: KeyboardEvent<HTMLDivElement>) => void;
  /** Any entry activated: the panel closes behind the navigation or the action. */
  onPick: () => void;
  onSwitchAccount?: () => void;
  onSignOut: () => void;
};

export function MenuPanel({ id, panelRef, phase, identity, links, groups, onKeyDown, onPick, onSwitchAccount, onSignOut }: MenuPanelProps) {
  const t = chipTranslator();
  const shown = phase === "open";
  return (
    <div
      ref={panelRef}
      id={id}
      data-state={shown ? "open" : "closed"}
      hidden={phase === "closed"}
      inert={!shown}
      onKeyDown={onKeyDown}
      // Inline, not the `hidden` utility: a host's `.flex` in a higher layer would beat ours,
      // and the attribute alone loses to `flex` without preflight.
      style={phase === "closed" ? { display: "none" } : undefined}
      className={PANEL}
    >
      <ProfileRow identity={identity} links={links} onPick={onPick} />
      {links.manage && (
        <a href={links.manage} onClick={onPick} className={ENTRY}>
          {t("auth.manageAccount", "Manage account")}
        </a>
      )}
      {groups.map((group, i) => {
        const labelId = `${id}-group-${i}`;
        return (
          <Fragment key={group.id}>
            <hr role="separator" className={RULE} />
            <div role={group.label ? "group" : undefined} aria-labelledby={group.label ? labelId : undefined} className="flex flex-col">
              {group.label && (
                <p id={labelId} className="m-0 px-2 py-1 text-xs text-ink-soft">
                  {group.label}
                </p>
              )}
              {group.items.map((item) => (
                <a
                  key={item.id}
                  href={item.href}
                  aria-current={item.current ? "page" : undefined}
                  target={item.external ? "_blank" : undefined}
                  rel={item.external ? "noopener noreferrer" : undefined}
                  onClick={onPick}
                  className={cn(ENTRY, "aria-[current=page]:bg-ink/5 aria-[current=page]:font-semibold")}
                >
                  {item.label}
                </a>
              ))}
            </div>
          </Fragment>
        );
      })}
      <hr role="separator" className={RULE} />
      {links.switchAccount && (
        <a
          href={links.switchAccount}
          onClick={() => {
            onSwitchAccount?.();
            onPick();
          }}
          className={ENTRY}
        >
          {t("auth.switchAccount", "Switch account")}
        </a>
      )}
      <button
        type="button"
        onClick={() => {
          onPick();
          onSignOut();
        }}
        className={ENTRY}
      >
        {t("auth.signOut", "Sign out")}
      </button>
    </div>
  );
}

/**
 * Name and email, as an explicit way in: "Cabinet" (or "Profile" inside the cabinet) and a
 * chevron on the right, so it reads as a row you can press and not as a caption. With no
 * destination it is plain text — no label, no chevron, nothing that promises a link.
 */
function ProfileRow({ identity, links, onPick }: Pick<MenuPanelProps, "identity" | "links" | "onPick">) {
  const t = chipTranslator();
  const who = (
    <span className="flex min-w-0 flex-1 flex-col">
      <span className="truncate font-semibold">{identity.label}</span>
      {identity.email && <span className="truncate text-ink-soft">{identity.email}</span>}
    </span>
  );
  if (!links.profile) return <div className="flex min-h-11 flex-col justify-center px-2 py-1.5 text-sm text-ink">{who}</div>;
  return (
    <a href={links.profile.href} onClick={onPick} className={ENTRY}>
      {who}
      <span className="flex shrink-0 items-center gap-0.5 text-xs text-ink-soft">
        {links.profile.kind === "profile" ? t("ui.profile", "Profile") : t("auth.cabinet", "Cabinet")}
        <ChevronRight className="size-4" aria-hidden />
      </span>
    </a>
  );
}
