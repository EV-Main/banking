"use client";

// What a screen shows when a read it needed never arrived.
//
// Ten views each derived a read's failure — most as `data ? null : (read.error?.message ??
// null)`, some with an action error folded in first — and then hand-rolled the banner for
// it. Nothing kept those banners in step, so the same event was reported in three different
// looks depending on which screen you were on, and a fix to one never reached the others.
// Deriving the message stays with the caller; only the looks are stated once, here.
//
// Two forms, because the console and the investor screens report a failure at different
// weights and both are deliberate:
//
//   `inline` — one destructive line under the page header. The admin console's screens are
//              dense grids of figures; a bordered block above them pushes the whole page
//              down for a message that is usually one clause long.
//   `alert`  — uikit's `Alert`, with a heading. The investor screens address the reader
//              ("Couldn't load your positions"), so the message needs a subject and the
//              block gives it one.
//
// `onRetry` boxes the inline form. A button cannot sit in a bare line of text without the
// two reading as unrelated things that happen to be adjacent, so the row and the button
// get a border around them and become one block.
//
// It renders its own `StaggerItem` for the same reason `AdminHeader` does: this is a
// section of the page, it arrives with the rest of them, and a caller that had to remember
// to wrap it would be a caller that could forget. Nothing is wrapped in a new element —
// the inline form animates the `<p>` itself (see `shared/ui/motion`, where the rule and
// the layout it protects are spelled out).
//
// One page-level banner is deliberately not on this: wallet Activity reports its failure
// as bare text with no icon. That is a difference in what is drawn, not in how it is
// built, so aligning it is a UI decision rather than something to smuggle in here.

import { RefreshCw, TriangleAlert } from "lucide-react";
import type { ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";
import { Alert, AlertDescription, AlertTitle, Button } from "@evinvest/uikit";

import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { StaggerItem } from "@/shared/ui/motion";

interface ResourceErrorBase {
  /** Placement, on whichever element this form's root is. */
  className?: string;
}

// What is being reported, in one of two shapes, and the union is the point of the
// distinction rather than a convenience:
//
//   `error`   — the thrown failure itself. Preferred. An error's wording was fixed in
//               English down in the fetch layer, where no translator exists; the key it
//               carries can still be resolved here, at paint time, in the reader's
//               language. This is the render boundary `api-client.ts` names, and passing
//               the error rather than a string is what lets it do its job.
//   `message` — copy the caller composed itself. For the failures that are not a thrown
//               request: a form that rejected its own input, a sentence with a value
//               interpolated into it. Already translated by whoever built it.
//
// Passing `error.message` as `message` type-checks and is exactly the mistake this split
// exists to make visible: it pins the banner to English forever.
//
// Note what the ten current callers actually do: they pass `message`, having already run
// `errorMessage(err, t)` themselves. That is not the mistake above — it is the correct
// second branch. Every one of them either folds an action error in ahead of the read's
// (`actionError ?? …`) or gates on `data` first, and several also pass a translated
// `title`, so they hold a translator regardless and composing the string is the simpler
// call. `error` is here for the caller that has nothing but the thrown failure; it is not
// the path the console happens to take today.
type ResourceErrorSource =
  | { error: unknown; message?: never }
  | { message: ReactNode; error?: never };

// The two forms take different props, and the union says so rather than documenting it: an
// `alert` has no retry button and an `inline` line has no heading, so asking for either is a
// type error at the call site instead of a prop that silently does nothing.
export type ResourceErrorProps = ResourceErrorBase &
  ResourceErrorSource &
  (
    | {
        variant?: "inline";
        title?: never;
        /** Re-run the read. Present only where the screen has a read worth retrying by itself. */
        onRetry?: () => void;
        /** The retry is in flight: the button is disabled and its icon spins. */
        retrying?: boolean;
      }
    | {
        variant: "alert";
        /** Heading for the `alert` form. */
        title?: ReactNode;
        onRetry?: never;
        retrying?: never;
      }
  );

/**
 * The banner for a read that failed with nothing to show in its place.
 *
 * Whether there is anything to report stays with the caller — only it knows whether stale
 * data is still on screen, and a failed *refresh* over figures the reader can already see
 * belongs beside them rather than in place of them.
 *
 * Given an `error`, this is where it becomes words, and it is the only place: every banner
 * in the cabinet lands here, so one call to `errorMessage` translates all of them and no
 * view has to hold a translator to report a failure it did not cause.
 */
export function ResourceError({ message, error, variant = "inline", title, onRetry, retrying = false, className }: ResourceErrorProps) {
  const t = useT();
  const text = error === undefined ? message : errorMessage(error, t);

  if (variant === "alert") {
    return (
      <StaggerItem className={className}>
        <Alert variant="destructive">
          <TriangleAlert className="size-4" />
          {title && <AlertTitle>{title}</AlertTitle>}
          <AlertDescription>{text}</AlertDescription>
        </Alert>
      </StaggerItem>
    );
  }

  if (onRetry) {
    return (
      <StaggerItem className={cn("flex flex-wrap items-center gap-3 rounded-lg border border-accent-error/40 bg-accent-error/10 px-3 py-2.5", className)}>
        {/* The icon holds its size here: beside the button it is the first thing squeezed
            when the message is long, and a half-width triangle reads as a rendering fault. */}
        <p className="flex min-w-0 items-center gap-2 text-sm text-accent-error">
          <TriangleAlert className="size-4 shrink-0" /> {text}
        </p>
        {/* i18n-max: 16 — the uikit Button is shrink-0, so its label is taken out of the
            message's share of the row before the row is allowed to wrap. */}
        <Button type="button" variant="outline" size="sm" disabled={retrying} onClick={onRetry}>
          <RefreshCw className={retrying ? "size-4 animate-spin" : "size-4"} /> {t("status.tryAgain")}
        </Button>
      </StaggerItem>
    );
  }

  return (
    <StaggerItem as="p" className={cn("flex items-center gap-2 text-sm text-accent-error", className)}>
      <TriangleAlert className="size-4" /> {text}
    </StaggerItem>
  );
}
