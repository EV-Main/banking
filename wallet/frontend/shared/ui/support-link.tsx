"use client";

import { useT } from "@evinvest/i18n/react";

import { SUPPORT_EMAIL } from "@/shared/config/support";
import { cn } from "@/shared/lib/cn";

/**
 * The `mailto:` for "contact support", built in one place. The address is encoded, not
 * interpolated: `?`/`&` in it would become the query half of the link — a pre-filled letter
 * to a third party behind a link the reader was told is support (`kyc-contract` refuses
 * that shape on the way in; this is the same rule at the point of use).
 */
export function supportHref({ contact = SUPPORT_EMAIL, subject }: { contact?: string; subject?: string } = {}): string {
  const base = `mailto:${encodeURIComponent(contact)}`;
  return subject ? `${base}?subject=${encodeURIComponent(subject)}` : base;
}

/**
 * The one way out of a sentence that ends in a refusal: "Contact <mailbox>", linking to the
 * mailbox the operators actually run (`@/shared/config/support`) — or to the address the
 * identity plane sent, when it sent one. A screen that says "we can't do this" and shows no
 * way to reach anyone is the dead end #386 closes, so every such sentence renders this
 * beside it rather than a hand-rolled anchor of its own.
 */
export function SupportLink({ contact = SUPPORT_EMAIL, subject, className }: { contact?: string; subject?: string; className?: string }) {
  const t = useT();
  return (
    <a href={supportHref({ contact, subject })} className={cn("rounded-sm font-medium text-primary-ink underline underline-offset-2 outline-none focus-visible:ring-2 focus-visible:ring-ring", className)}>
      {t("profile.kyc.contact", "Contact {contact}", { contact })}
    </a>
  );
}
