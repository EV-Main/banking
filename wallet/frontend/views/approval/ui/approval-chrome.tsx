"use client";

// The framing and the shared states of the emailed approval page; the signed-in payment
// answer borrows them.
//
// These pages are unlike everything else in the wallet host: no session, no rail, no shell, and
// for most readers exactly one visit. That makes the non-happy states the majority of the
// design surface rather than an afterthought — a link that has expired, been used, or been
// closed is a page someone will genuinely land on, having done nothing wrong, with no
// navigation to fall back to. So the outcomes live here as first-class components built on
// uikit's `Empty` (AGENTS.md § Frontend design rules: never a bare grey sentence in a blank
// box), and every page composes the same ones.

import { Clock, Link2Off, ShieldX, Unplug } from "lucide-react";
import { useEffect, useRef, type ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";
import {
  Button,
  CardTitle,
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  Field,
  FieldDescription,
  FieldError,
  FieldLabel,
  Input,
  Skeleton,
  Spinner,
} from "@evinvest/uikit";

import { Logo } from "@/shared/ui/logo";
import { cn } from "@/shared/lib/cn";

/**
 * The page frame: centered column, brand mark, and nothing else.
 *
 * `--ev-shell-offset` is the one contract the wallet host has with the conductor's header, and
 * it is honoured here as everywhere else — these pages are chromeless, not context-free.
 *
 * A `<main>` because nothing else on these routes provides one: the `(public)` layout is
 * a bare `div` by design, and a reader arriving by screen reader with no rail and no nav
 * has the landmark as the one way to jump to the content.
 */
export function ApprovalPage({ children }: { children: ReactNode }) {
  return (
    <main className="flex min-h-[calc(100dvh-var(--ev-shell-offset,0px))] justify-center px-4 py-10 lg:py-16">
      <div className="flex w-full max-w-160 flex-col gap-6">
        <Logo className="h-8 w-auto text-ink" />
        {children}
      </div>
    </main>
  );
}

/**
 * The page's one heading. `CardTitle` is a `div`, and a page whose only title is a styled
 * `div` has no `h1` for a reader to land on — so the heading role is put back here, once,
 * rather than remembered on each of the three pages.
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
 * The secret code, and the only control that can turn this page into a decision.
 *
 * The code is what makes a vote a deliberate human act: mail scanners follow every link in
 * a message, so the GET behind this page is inert and the code is what a scanner cannot
 * type. The field says that plainly rather than presenting itself as a second
 * password, because a reader who thinks it is one will go looking for it in a password
 * manager instead of in the email they are already reading.
 */
const CODE_FIELD_ID = "approval-code";
const CODE_HINT_ID = "approval-code-hint";
const CODE_ERROR_ID = "approval-code-error";

export function CodeField({
  value,
  onChange,
  disabled,
  attemptsRemaining,
}: {
  value: string;
  onChange: (next: string) => void;
  disabled: boolean;
  /** Set only after a code was rejected — never a countdown we maintain ourselves. */
  attemptsRemaining: number | null;
}) {
  const t = useT();
  return (
    <Field>
      <FieldLabel htmlFor={CODE_FIELD_ID}>{t("approval.codeLabel", "Code from your email")}</FieldLabel>
      <Input
        id={CODE_FIELD_ID}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        disabled={disabled}
        // Not a password: it is printed in the message the reader has open, and masking it
        // would only make it easier to mistype. `one-time-code` keeps password managers
        // from offering to save it as a credential.
        autoComplete="one-time-code"
        autoCapitalize="characters"
        autoCorrect="off"
        // The alphabet is Crockford base32 minus I/L/O/U — letters and digits — so this is
        // a text keyboard, not the numeric one `numeric` would raise on a phone.
        inputMode="text"
        spellCheck={false}
        maxLength={16}
        aria-invalid={attemptsRemaining !== null}
        // Points at whichever of the two is rendered below, so the hint (and, more
        // importantly, the burned-an-attempt count) is announced with the field rather
        // than being visible only to people who can see it.
        aria-describedby={attemptsRemaining === null ? CODE_HINT_ID : CODE_ERROR_ID}
        placeholder={t("approval.codePlaceholder", "e.g. K3M9XR2Q7T")}
        className="font-mono-tech text-base uppercase tracking-widest tabular-nums"
      />
      {attemptsRemaining === null ? (
        <FieldDescription id={CODE_HINT_ID}>{t("approval.codeHint", "Ten characters, printed in the message that brought you here. It is what tells us a person opened this, and not an automatic link checker.")}</FieldDescription>
      ) : (
        // The count is the server's, read back from the authoritative summary — never a
        // number this page decremented for itself. The attempt is recorded before the
        // comparison, in the same transaction, so ours would be a guess.
        //
        // `role="alert"` because losing an attempt is news: four wrong codes and the link
        // burns, and a reader who cannot see the field turning red gets no other warning.
        <FieldError id={CODE_ERROR_ID} role="alert" className="tabular-nums">
          {t("approval.codeWrong", "That code didn't match. {n, plural, one {# attempt remains} other {# attempts remain}} before this link closes for good.", { n: attemptsRemaining })}
        </FieldError>
      )}
    </Field>
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

/**
 * The one answer for every token that cannot be used.
 *
 * Unknown, expired, already-used and burned all arrive here as one identical 404, and this
 * component deliberately does not guess between them: telling them apart is an enumeration
 * oracle, and it is closed on purpose. The copy says so — that the reason is
 * not knowable from this page is more useful to a reader than a confident wrong guess, and
 * it points at the person who can actually fix it.
 */
export function ApprovalUnavailable() {
  const t = useT();
  return (
    <ApprovalOutcome
      icon={<Link2Off />}
      title={t("approval.gone.title", "This link can no longer be used")}
      description={t("approval.gone.body", "Approval links work once and expire after 72 hours. This one may have been used already, run out of time, or been closed — which of those it was cannot be told from here. Ask whoever opened the request to send a new one.")}
    />
  );
}

/**
 * The deadline passed while nobody answered.
 *
 * A settled state in its own right, and not a disabled form: a reader arriving four hours
 * late should be told what happened, not handed a control they cannot use and left to work
 * out why it is greyed out.
 */
export function ApprovalExpired() {
  const t = useT();
  return (
    <ApprovalOutcome
      icon={<Clock />}
      title={t("approval.expired.title", "The deadline for this request has passed")}
      description={t("approval.expiredNote", "The deadline for this request has passed, so it can no longer be answered. Nothing was decided.")}
    />
  );
}

/** The token burned: five wrong codes, and every owner has been told. */
export function ApprovalBurned() {
  const t = useT();
  return (
    <ApprovalOutcome
      icon={<ShieldX />}
      tone="text-accent-error"
      title={t("approval.burned.title", "This link has been closed")}
      description={t("approval.burned.body", "The code was entered incorrectly too many times, so the link was closed and every owner has been told. Nothing was approved and no money has moved. Ask whoever opened the request to send a new one.")}
    />
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

/**
 * A read that failed for an ordinary reason — offline, a 500.
 *
 * Deliberately NOT the "this link is dead" state. The token may be perfectly good, and
 * telling someone their one-shot approval link is broken when it is not is the more
 * expensive of the two mistakes, so this one offers a retry and says nothing about the
 * link itself.
 */
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
