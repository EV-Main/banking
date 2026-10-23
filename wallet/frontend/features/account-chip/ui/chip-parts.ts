import { cn } from "@/shared/lib/cn";

// The chip's controls are hand-written so the bundle stays free of the uikit Button, which
// means the keyboard focus ring has to be written out too.
export const CHIP_FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-ring";

// The chip's CSS sits in the `reamfe` layer, BELOW every layer of the host, so any rule of
// the host that matches a chip element wins on the properties it sets — whatever its layer
// and however plain its selector. Two kinds bite:
//   • Resets in the host's `base`: Tailwind's preflight zeroes padding, margin and border on
//     every element (`*`), gives <a> `color: inherit`, and gives <button> `font: inherit`,
//     `color: inherit`, `border-radius: 0`, `background-color: transparent`, `opacity: 1`.
//     On a real host the chip's buttons lost their hover fill this way; padding and borders
//     survived only where the host happened to generate the same utility itself.
//   • The same utility name in the host's `utilities` (`opacity-0`, `bg-transparent`): it
//     outranks our variants on that element, so an open panel stays invisible.
// Hence two rules, both independent of the host's layer order:
//   1. Every chip class on a property a reset touches on that element is important (`px-2!`,
//      `rounded-md!` on a/button, `text-sm!`/`text-ink!` on a/button, `border!`, `my-1!`):
//      an important declaration in an EARLIER layer beats the host's normal ones in every
//      later layer. The `!` class names exist only in this bundle, so nothing leaks onto the
//      host's own elements. Once a property's base is important, each of its state variants
//      must be too (`hover:bg-ink/5!`), or the base outranks the state.
//   2. A look switched by state never takes its base from a stock utility: the base is the
//      default or an arbitrary property, and the state is the variant (menu-panel.tsx).
// Layout-only classes (flex, gap, size, w-*) are left plain: no reset touches them.
//
// Tailwind v4 already scopes `hover:` to `@media (hover: hover)`, so a tap on a phone does
// not leave the row lit; `active:` is the touch feedback instead. Manipulation drops the
// double-tap-zoom wait, and the grey iOS tap flash would double the active fill.
export const CHIP_PRESS = cn(
  "[cursor:pointer]! touch-manipulation transition-colors [-webkit-tap-highlight-color:transparent] [background-color:transparent]!",
  "hover:bg-ink/5! active:bg-ink/10!",
);

export type MenuIdentity = { label: string; email: string | null; initials: string };

export type MenuLinks = {
  /**
   * The name/email row of the panel and what it is labelled: `cabinet` from outside the
   * wallet host zone (and on hosts), `profile` inside it. Null renders the row as plain text.
   */
  profile: { href: string; kind: "cabinet" | "profile" } | null;
  manage: string | null;
  switchAccount: string | null;
};
