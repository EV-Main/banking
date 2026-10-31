"use client";

// The framing and the shared states of the signed-in payment answer. A link that has
// expired or been answered is a page someone will genuinely land on, so the outcomes live
// here as first-class components built on uikit's `Empty` (AGENTS.md § Frontend design
// rules: never a bare grey sentence in a blank box).

import { Unplug } from "lucide-react";
import { useEffect, useRef, type ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button, CardTitle, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Skeleton, Spinner } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";

/**
 * The page's one heading. `CardTitle` is a `div`, and a page whose only title is a styled
 * `div` has no `h1` for a reader to land on — so the heading role is put back here, once,
 * rather than remembered on each page.
 */
export function ApprovalTitle({ children }: { children: ReactNode }) {
  return (
    <CardTitle role="heading" aria-level={1} className="text-xl">
      {children}
    </CardTitle>
  );
}

/**
 * The caption above a value. Sentence case and muted, matching the house `Row` in
 * `views/wallet/ui/withdraw-view.tsx` — a column of uppercase tracking-wider labels reads
 * as a generated spec sheet rather than as something written for a reader.
 */
export function FieldCaption({ children }: { children: ReactNode }) {
  return <span className="text-xs font-medium text-ink-soft">{children}</span>;
}

/** A labelled figure. The value is `tabular-nums` because most of these are numbers or times. */
export function DetailRow({ label, value, mono = false, tone }: { label: string; value: ReactNode; mono?: boolean; tone?: string }) {
  return (
    <div className="flex flex-col gap-1 sm:flex-row sm:items-baseline sm:justify-between sm:gap-4">
      <span className="shrink-0 text-xs font-medium text-ink-soft">{label}</span>
      <span className={cn("min-w-0 text-sm font-medium tabular-nums", mono && "break-all font-mono-tech", tone ?? "text-ink")}>{value}</span>
    </div>
  );
}

/**
 * The destination address, in full.
 *
 * Its own component so that no screen can quietly decide to shorten it. A `0x1234…abcd` in
 * an approval flow is an invitation to approve the wrong address — the one thing an owner
 * is here to check is the one thing that must never be elided. `break-all` rather than truncation: it wraps, it never hides.
 */
export function FullAddress({ label, address }: { label: string; address: string }) {
  return (
    <div className="flex flex-col gap-1.5">
      <FieldCaption>{label}</FieldCaption>
      <p className="break-all rounded-lg border border-border bg-secondary px-3.5 py-3 font-mono-tech text-base leading-relaxed text-ink">
        {address}
      </p>
    </div>
  );
}

/**
 * A settled or unusable outcome, with an icon, a title and a plain explanation.
 *
 * It takes focus on mount. Every one of these replaces the form the reader was just using,
 * so without this the focused element is destroyed and focus falls to `<body>` — a screen
 * reader announces nothing, and the one sentence saying what the answer did goes
 * unread by the person who most needs it. `tabIndex={-1}` makes it focusable
 * programmatically without adding it to the tab order.
 */
export function ApprovalOutcome({
  icon,
  title,
  description,
  children,
  tone,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  children?: ReactNode;
  tone?: string;
}) {
  const landing = useRef<HTMLDivElement>(null);
  useEffect(() => {
    landing.current?.focus();
  }, []);
  return (
    <Empty
      ref={landing}
      tabIndex={-1}
      role="status"
      className="rounded-xl border border-border bg-card p-6 outline-none focus-visible:ring-2 focus-visible:ring-ring md:p-8"
    >
      <EmptyHeader>
        <EmptyMedia variant="icon" className={tone}>
          {icon}
        </EmptyMedia>
        <EmptyTitle>{title}</EmptyTitle>
        <EmptyDescription className="text-balance">{description}</EmptyDescription>
      </EmptyHeader>
      {children && <EmptyContent>{children}</EmptyContent>}
    </Empty>
  );
}

/** What the page is while the summary is on its way. */
export function ApprovalSkeleton() {
  return (
    <div className="flex flex-col gap-4 rounded-xl border border-border bg-card p-6">
      <Skeleton className="h-6 w-56" />
      <Skeleton className="h-4 w-72" />
      <Skeleton className="h-32 w-full rounded-lg" />
      <Skeleton className="h-10 w-full rounded-lg" />
      <Skeleton className="h-11 w-full rounded-lg" />
    </div>
  );
}

/** A read that failed for an ordinary reason — offline, a 500 — so it offers a retry. */
export function ApprovalUnreachable({ onRetry, retrying }: { onRetry: () => void; retrying: boolean }) {
  const t = useT();
  return (
    <ApprovalOutcome icon={<Unplug />} title={t("approval.loadFailed", "We couldn't load this request")} description={t("approval.loadFailedHint", "The link itself may be perfectly fine — this looks like a problem reaching us. Try again in a moment.")}>
      <Button variant="outline" onClick={onRetry} disabled={retrying}>
        {retrying && <Spinner aria-hidden />}
        {t("status.tryAgain", "Try again")}
      </Button>
    </ApprovalOutcome>
  );
}
