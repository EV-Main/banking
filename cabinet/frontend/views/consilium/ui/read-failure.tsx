"use client";

// What a card on this page shows when the read behind it did not arrive.
//
// Three things it is not, each of which this page shipped:
//
//   · Not nothing. A title and a description above an empty box is the state the reader
//     cannot act on and cannot even name — every card here shows a skeleton, a designed
//     zero state, or this.
//   · Not "Not found." A raw status turned into prose tells an owner what an HTTP client
//     saw, never what it means for them. What it means is that the page cannot show who
//     owns the fund or what is being decided, and that is what these read.
//   · Not a bare grey sentence in a blank box (AGENTS.md § Frontend design rules). It is
//     uikit's `Empty`, like every other zero state in the cabinet, and it carries the one
//     action that can change the situation.
//
// **No motion, deliberately.** `ResourceError` — what these cards used before — renders a
// `StaggerItem`, which carries no `initial`/`animate` of its own and takes both from the
// `Stagger` above it. That is right for a section present when the page arrives, and it is
// a bet for one that mounts afterwards: a failure appears, by definition, once the sequence
// it would have joined has finished, so its visibility depends on the variant tree being
// re-run rather than on anything this component does. A message saying the reader cannot
// trust what is on screen is not the place to take that bet. The sections of this page
// still arrive through `Stagger` — this is a child of one, not one of them.

import { CloudOff, RefreshCw } from "lucide-react";
import type { ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";
import { Button, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";

export interface ReadFailureProps {
  /** What did not load, in the reader's terms — not the status the fetch saw. */
  title: ReactNode;
  /** What it means for them: what the page cannot show, and what not to trust until it does. */
  body: ReactNode;
  /** Re-run the read (or reads) this failure is about. */
  onRetry: () => void;
  /** The retry is in flight: the button is disabled and its icon spins. */
  retrying?: boolean;
  className?: string;
}

export function ReadFailure({ title, body, onRetry, retrying = false, className }: ReadFailureProps) {
  const t = useT();
  return (
    // Solid and tinted, against the dashed frame every zero state on this page uses: "this
    // did not load" and "there is nothing here yet" are opposite answers, and the reader
    // should be able to tell them apart before reading a word.
    <Empty className={cn("border border-solid border-accent-error/40 bg-accent-error/10 md:p-6", className)}>
      <EmptyHeader>
        <EmptyMedia variant="icon" className="text-accent-error">
          <CloudOff />
        </EmptyMedia>
        <EmptyTitle>{title}</EmptyTitle>
        {/* `text-ink`, not the muted default: muted on a tinted ground is the
            contrast failure AGENTS.md names, and it is measured under AA here. */}
        <EmptyDescription className="text-ink">{body}</EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        {/* i18n-max: 16 — the uikit Button is shrink-0. */}
        <Button type="button" variant="outline" disabled={retrying} onClick={onRetry}>
          <RefreshCw className={retrying ? "size-4 animate-spin" : "size-4"} />
          {t("status.tryAgain")}
        </Button>
      </EmptyContent>
    </Empty>
  );
}
