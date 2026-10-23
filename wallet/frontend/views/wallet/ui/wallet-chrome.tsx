"use client";

import type { ReactNode } from "react";

import { cn } from "@/shared/lib/cn";
import { MobileAppBar } from "@/shared/ui/mobile-appbar";
import { PageFrame } from "@/shared/ui/page-frame";

// Shared chrome for the four wallet screens (Figma `cabinet/wallet/*` desktop +
// `cabinet/mobile/wallet/*`): the cabinet's page frame with the mobile app bar the profile
// and settings screens use, so a wallet screen is titled the same way at both breakpoints
// as every other investor screen. Each screen supplies its own sections as `StaggerItem`s.
export function WalletScreen({ title, subtitle, back, actions, entrance, children }: { title: string; subtitle?: string; back?: `/${string}`; actions?: ReactNode; entrance?: boolean; children: ReactNode }) {
  return (
    <PageFrame title={title} description={subtitle} actions={actions} entrance={entrance} appBar={<MobileAppBar title={title} backHref={back} entrance={entrance} />}>
      {children}
    </PageFrame>
  );
}

export const WALLET_CARD = "rounded-xl border border-border bg-card";

// The all-caps field/section label used across the wallet cards. With `htmlFor` it is a
// real label pinned to one control; a `<label>` wrapped around the label text, a tip button
// and the input binds to the FIRST labelable element — the tip — and leaves the input
// with no accessible name (the placeholder gets read instead).
export function FieldLabel({ children, className, htmlFor }: { children: ReactNode; className?: string; htmlFor?: string }) {
  const cls = cn("flex items-center gap-1.5 text-xs font-medium text-ink-soft", className);
  if (htmlFor) {
    return (
      <label htmlFor={htmlFor} className={cls}>
        {children}
      </label>
    );
  }
  return <span className={cls}>{children}</span>;
}
