import { cn } from "@/shared/lib/cn";

// The chip's controls are hand-written so the bundle stays free of the uikit Button, which
// means the keyboard focus ring has to be written out too.
export const CHIP_FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-ring";

// Tailwind v4 already scopes `hover:` to `@media (hover: hover)`, so a tap on a phone does
// not leave the row lit; `active:` is the touch feedback instead. Manipulation drops the
// double-tap-zoom wait, and the grey iOS tap flash would double the active fill.
export const CHIP_PRESS = cn(
  "cursor-pointer touch-manipulation transition-colors [-webkit-tap-highlight-color:transparent]",
  "hover:bg-ink/5 active:bg-ink/10",
);

export type MenuIdentity = { label: string; email: string | null; initials: string };

export type MenuLinks = {
  /**
   * The name/email row of the panel and what it is labelled: `cabinet` from outside the
   * cabinet (and on hosts), `profile` inside it. Null renders the row as plain text.
   */
  profile: { href: string; kind: "cabinet" | "profile" } | null;
  manage: string | null;
  switchAccount: string | null;
};
