import { Item, ItemContent, ItemDescription, ItemMedia, ItemTitle } from "@evinvest/uikit";
import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

/**
 * One step of the verification story — an icon, a title and a sentence.
 *
 * `Item` and not three hand-written `div`s: the kit owns this shape, and what is overridden
 * here is only the frame —
 * the chip keeps the surrounding surface's rounding, and the row drops the padding it would
 * carry as a standalone list item.
 *
 * Shared between the verification dialog and the login page's "what happens next" (#391)
 * so the story a newcomer is told before signing in and the one they meet after look like
 * one story. `asChild` lets a caller render it as an `<li>` inside a real list.
 */
export function Step({ icon: Icon, title, body, asChild }: { icon: LucideIcon; title: string; body: string; asChild?: boolean }) {
  const content: ReactNode = (
    <>
      <ItemMedia variant="icon" className="rounded-lg border-0 bg-secondary text-ink-mid">
        <Icon aria-hidden />
      </ItemMedia>
      <ItemContent className="min-w-0">
        <ItemTitle className="font-semibold text-ink">{title}</ItemTitle>
        <ItemDescription className="line-clamp-none leading-snug">{body}</ItemDescription>
      </ItemContent>
    </>
  );
  return (
    <Item asChild={asChild} className="items-start gap-3 rounded-none p-0">
      {asChild ? <li>{content}</li> : content}
    </Item>
  );
}
