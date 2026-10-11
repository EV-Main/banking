import { cn } from "@/shared/lib/cn";

// The chip's controls are hand-written so the bundle stays free of the uikit Button, which
// means the keyboard focus ring has to be written out too.
export const CHIP_FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-ring";

// The chip's CSS sits in the `reamfe` layer, BELOW every layer of the host. A host that
// ships the same utility name (transparent background, zero opacity, pointer cursor) in its own
// `utilities` layer therefore beats any variant of ours on the same element — the hover fill
// or the open state never shows. So a property the chip varies by state never gets its base
// value from a stock utility: the base is an arbitrary property (a class name no host
// generates) or is left to the default, and every state is a variant.
//
// Tailwind v4 already scopes `hover:` to `@media (hover: hover)`, so a tap on a phone does
// not leave the row lit; `active:` is the touch feedback instead. Manipulation drops the
// double-tap-zoom wait, and the grey iOS tap flash would double the active fill.
export const CHIP_PRESS = cn(
  "[cursor:pointer] touch-manipulation transition-colors [-webkit-tap-highlight-color:transparent] [background-color:transparent]",
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
