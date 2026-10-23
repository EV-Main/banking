"use client";

import type { CSSProperties, ElementType, ReactNode } from "react";

import { cn } from "@/shared/lib/cn";
import { SECTION_STAGGER, Stagger, StaggerItem } from "@/shared/ui/motion";

// The frame every investor screen sits in, decided once. Eight screens used to carry their
// own inset, max-width and title scale — `px-4 pt-5` beside `container max-w-5xl py-10`
// beside `max-w-282 px-4 py-6`, an h1 at 2xl on one page and 3xl on the next — so moving
// between them read as moving between products. The dashboard's frame is the canonical
// one: full width (the shell's rail already bounds the column), 16px inset on a phone and
// 32px from `lg`, the title one step above the card titles.

/** The page inset: the dashboard's, which every other screen now shares. */
export const PAGE_PAD = "px-4 pb-6 pt-5 lg:px-8 lg:pb-7 lg:pt-6";
/** The same inset as margins, for a block that sits above the frame (Home's onboarding path). */
export const PAGE_INSET_X = "mx-4 lg:mx-8";
export const PAGE_INSET_TOP = "mt-5 lg:mt-6";
/** A reading measure for screens that are prose and cards rather than a grid: at 1920px an
 *  unbounded product page ran its paragraphs to ~150 characters a line. */
export const PAGE_CONTENT_WIDTH = "max-w-5xl";
/** The rhythm between a page's sections. */
export const PAGE_GAP = "gap-4 lg:gap-6";
/** The page title. One step for every screen, so none of them shouts louder than another. */
export const PAGE_TITLE = "text-2xl font-semibold leading-tight text-ink";
/** The line under the title — what the screen is for, in a sentence. */
export const PAGE_DESCRIPTION = "text-sm text-ink-soft";

/**
 * The tracked-uppercase label above a figure, a group or a section. `muted` is the
 * section label (the settings groups, a day in the timeline); `accent` announces a
 * hero figure (the portfolio value, the invested band). Not the uikit's `Eyebrow`, and
 * not named after it: that one is the landing's — a 10–11.5px brand-coloured label with
 * `tracking-[0.15em]` and no `uppercase` of its own — so adopting it would have meant
 * overriding four of its five utilities, and a same-named local would shadow the export.
 */
export function SectionLabel({ as: Comp = "p", tone = "muted", className, children }: { as?: ElementType; tone?: "muted" | "accent"; className?: string; children: ReactNode }) {
  return <Comp className={cn("text-xs font-semibold uppercase tracking-widest", tone === "accent" ? "text-primary-ink" : "text-ink-soft", className)}>{children}</Comp>;
}

export interface PageHeadingProps {
  title: string;
  description?: string;
  eyebrow?: string;
  /** Trailing controls — the screen's own actions, outline unless the page has no other solid CTA. */
  actions?: ReactNode;
  className?: string;
}

/** The title row: eyebrow → title → description on the left, the screen's actions on the right. */
export function PageHeading({ title, description, eyebrow, actions, className }: PageHeadingProps) {
  return (
    <StaggerItem as="header" className={cn("flex items-center justify-between gap-4", className)}>
      <div className="flex min-w-0 flex-col gap-1">
        {eyebrow && <SectionLabel tone="accent">{eyebrow}</SectionLabel>}
        <h1 className={PAGE_TITLE}>{title}</h1>
        {description && <p className={PAGE_DESCRIPTION}>{description}</p>}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2.5">{actions}</div>}
    </StaggerItem>
  );
}

export interface PageFrameProps extends Omit<PageHeadingProps, "title" | "className"> {
  /** The page title; a screen whose heading is its own composition (the product page) passes none. */
  title?: string;
  /**
   * The bar that titles the screen below `lg` (`MobileAppBar`). With one, the heading is
   * the desktop's alone and the sections arrive one step behind the bar, which plays first.
   */
  appBar?: ReactNode;
  headingClassName?: string;
  /** `full` is the dashboard's canon; `content` bounds prose-and-cards screens to a reading measure. */
  width?: "full" | "content";
  /** Layout for the sections — a column by default; the dashboard passes its grid. */
  className?: string;
  style?: CSSProperties;
  /**
   * Play the arrival. `false` for a screen whose figures were rendered on the server: the
   * entrance starts from transparent, so it would hide figures already in the HTML until
   * hydration and then delay them by its own length — a skeleton earned that, they did not.
   */
  entrance?: boolean;
  children: ReactNode;
}

/**
 * The frame: inset, section rhythm and the entrance, with the heading as the first section.
 * Each child is a `StaggerItem` and arrives in DOM order; anything passed as a bare element
 * still renders, it just arrives with the column rather than in sequence.
 */
export function PageFrame({ title, description, eyebrow, actions, appBar, headingClassName, width = "full", className, style, entrance = true, children }: PageFrameProps) {
  return (
    <>
      {appBar}
      <Stagger {...(entrance ? {} : { initial: false })} delay={appBar ? SECTION_STAGGER : 0} step={SECTION_STAGGER} className={cn("flex flex-col", PAGE_GAP, PAGE_PAD, width === "content" && PAGE_CONTENT_WIDTH, className)} style={style}>
        {title !== undefined && <PageHeading title={title} description={description} eyebrow={eyebrow} actions={actions} className={cn(appBar && "hidden lg:flex", headingClassName)} />}
        {children}
      </Stagger>
    </>
  );
}
