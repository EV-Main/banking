"use client";

import {
  InfoTip,
  InfoTipContent,
  InfoTipTrigger,
  SectionDescriptor,
} from "@evinvest/uikit";

import { useT } from "@evinvest/i18n/react";

import { useSession } from "@/shared/lib/use-session";

import { tips, type TipEntry, type TipKey } from "./catalog";

export interface TipAnchorProps {
  /** The catalog key to render. Checked against the catalog at compile time. */
  anchor: TipKey;
  className?: string;
}

/**
 * Renders the tip registered under `anchor`: an inline ⓘ toggletip for
 * `type: "input"` entries, or a section descriptor block for `type: "section"`.
 * The catalog says how a tip renders and who may see it; the words come from the
 * message catalogue under `tips.<anchor>.title` / `tips.<anchor>.body` — the uikit
 * engine still sees no content of its own.
 *
 * This is the one place tip copy is resolved, which is why the catalog could shed
 * its strings without every anchor site learning about `useT`. It is a Client
 * Component, so the hook finds the provider the root layout mounts.
 *
 * A catalog `roles` gate is enforced against the session role (cosmetic — server
 * authz stays authoritative), so an operator-only tip never renders for
 * investors. While the session is still loading a gated tip stays hidden.
 */
export function TipAnchor({ anchor, className }: TipAnchorProps) {
  const session = useSession();
  const t = useT();
  const entry: TipEntry = tips[anchor];

  if (entry.roles) {
    const role = session?.user?.role;
    if (!role || !entry.roles.includes(role)) return null;
  }

  const title = t(`tips.${anchor}.title`);
  const body = t(`tips.${anchor}.body`);

  if (entry.type === "section") {
    return (
      <SectionDescriptor title={title} className={className}>
        {body}
      </SectionDescriptor>
    );
  }

  return (
    <InfoTip>
      <InfoTipTrigger label={t("tips.a11y.about", { title })} className={className} />
      <InfoTipContent>
        <p className="text-ink font-medium">{title}</p>
        <p className="text-ink-soft mt-1">{body}</p>
      </InfoTipContent>
    </InfoTip>
  );
}
