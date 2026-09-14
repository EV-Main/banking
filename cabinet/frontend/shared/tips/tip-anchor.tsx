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
 * The catalog says how a tip renders, who may see it, and what it says; the uikit
 * engine still sees no content of its own.
 *
 * This is the one place tip copy is resolved, which is why no anchor site has to
 * learn about `useT`. It is a Client Component, so the hook finds the provider the
 * root layout mounts. Only the entry being rendered is formatted — the catalog
 * holds thunks, not strings.
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

  const title = entry.title(t);
  const body = entry.body(t);

  if (entry.type === "section") {
    return (
      <SectionDescriptor title={title} className={className}>
        {body}
      </SectionDescriptor>
    );
  }

  return (
    <InfoTip>
      <InfoTipTrigger label={t("tips.a11y.about", "About: {title}", { title })} className={className} />
      <InfoTipContent>
        <p className="text-ink font-medium">{title}</p>
        <p className="text-ink-soft mt-1">{body}</p>
      </InfoTipContent>
    </InfoTip>
  );
}
