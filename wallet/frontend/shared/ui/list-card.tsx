import { ChevronRight, type LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

import { Avatar, AvatarFallback, Badge } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";

// The card + row vocabulary the Settings and Profile surfaces are built from
// (Figma `cabinet/mobile/settings` node 498:259 and `cabinet/mobile/profile` node
// 503:266): a card on `card`, a compact section title, and rows split by
// hairlines. Section titles take `tracking-normal` — at this size the global
// `h1,h2,h3` tightening reads as cramped.
//
// Rows and hairlines stay local because uikit has no equivalent vocabulary. The
// card string stays local too: its call sites are a `<Link>` and a `<section>` as
// often as a `<div>`, which uikit's `Card` — a hard `<div>` carrying its own
// `gap-6 py-6 shadow-sm` — cannot be without being unset at every one. It is the
// same string as the wallet screens' `WALLET_CARD`.

export const CARD = "rounded-xl border border-border bg-card";

/** A tappable row — a button into a pushed screen, a link out. Hand-written (uikit has no
 *  list-row control), so it carries its own focus ring. */
export const ROW_INTERACTIVE = "flex min-w-0 items-center justify-between gap-3 rounded-md py-3.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring";

export function ListCard({ className, children }: { className?: string; children: ReactNode }) {
  return <section className={cn(CARD, "flex w-full min-w-0 flex-col px-4 pb-1.5 pt-1", className)}>{children}</section>;
}

export function ListCardTitle({ sub, children }: { sub?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-px pb-2 pt-3">
      <h2 className="text-sm font-semibold tracking-normal text-ink">{children}</h2>
      {sub && <p className="text-xs font-medium text-ink-soft">{sub}</p>}
    </div>
  );
}

export function Hairline() {
  return <div className="h-px shrink-0 bg-border" />;
}

export function Row({ className, children }: { className?: string; children: ReactNode }) {
  return <div className={cn("flex min-w-0 items-center justify-between gap-3 py-3.5", className)}>{children}</div>;
}

/** Row leading block: the title, with an optional caption a step down underneath. */
export function RowLabel({ title, sub }: { title: ReactNode; sub?: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-1 flex-col gap-0.5">
      <span className="text-sm font-medium text-ink">{title}</span>
      {sub && <span className="text-xs leading-snug text-ink-soft">{sub}</span>}
    </div>
  );
}

/**
 * Row trailing value — right-aligned and truncated rather than pushing the row wide.
 * It shares the row title's step, so the muted tone and the lighter weight are what
 * keep the label reading ahead of its value.
 */
export function RowValue({ className, children }: { className?: string; children: ReactNode }) {
  return <span className={cn("min-w-0 truncate text-right text-sm text-ink-soft", className)}>{children}</span>;
}

/** A stacked label-over-value row (Figma `card-Personal`). */
export function StackRow({ label, children }: { label: ReactNode; children: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-1 py-3">
      <span className="text-xs font-medium text-ink-soft">{label}</span>
      {children}
    </div>
  );
}

export function Chevron({ className }: { className?: string }) {
  return <ChevronRight className={cn("size-4 shrink-0 text-ink-soft", className)} aria-hidden />;
}

const PILL_TONE = {
  // The brand highlight — a fact worth a glance ("Connected", "This device"), not a verdict.
  accent: "bg-primary-ink/15 text-primary-ink",
  pending: "bg-accent-warn/15 text-accent-warn",
  neutral: "bg-ink/5 text-ink-soft",
  // The gain/loss pair (AGENTS.md § design), for a state that is a verdict rather than a wait.
  success: "bg-positive/15 text-positive",
  error: "bg-accent-error/15 text-accent-error",
} as const;

export type PillTone = keyof typeof PILL_TONE;

/** uikit's `Badge` in the pill silhouette these surfaces use — it sizes the icon too. */
export function Pill({ tone = "accent", icon: Icon, className, children }: { tone?: PillTone; icon?: LucideIcon; className?: string; children: ReactNode }) {
  return (
    <Badge className={cn("rounded-full font-semibold", PILL_TONE[tone], className)}>
      {Icon && <Icon aria-hidden />}
      {children}
    </Badge>
  );
}

/** The teal initials disc used for the account avatar; the caller sizes it. */
export function InitialsAvatar({ initials, className }: { initials: string; className?: string }) {
  return (
    <Avatar className={className} aria-hidden>
      <AvatarFallback className="bg-primary font-semibold text-on-primary">{initials}</AvatarFallback>
    </Avatar>
  );
}
