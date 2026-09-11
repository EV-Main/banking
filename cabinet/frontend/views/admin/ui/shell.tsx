"use client";

// Small presentational primitives shared across the admin-console views: the page
// header (eyebrow · title · subtitle · environment badge · action slot), a status dot,
// and a switch toggle. Kept dependency-light (no uncertain uikit exports) — plain
// buttons + tokens, matching the wallet view's idiom.

import type { ReactNode } from "react";

import { useT } from "@evinvest/i18n/react";

import { cn } from "@/shared/lib/cn";
import { usePlatform } from "@/shared/lib/use-platform";
import { SECTION_STAGGER, Stagger, StaggerItem } from "@/shared/ui/motion";
import { statusTone } from "@/views/admin/lib/format";

// Server-truthful environment (the BFF's APP_ENV); anything unrecognised — including
// the default "development" — reads as DEV. The badge carries a catalogue key rather than
// a finished word: this map is module scope, where no hook can run.
const ENV_BADGES: Record<string, { labelKey: string; tone: string }> = {
  production: { labelKey: "admin.env.prod", tone: "text-positive" },
  staging: { labelKey: "admin.env.staging", tone: "text-accent-warn" },
};

/**
 * The page column every admin screen is built in, and the entrance it arrives with.
 *
 * The console's nine screens all opened the same way — a `div` with the same padding
 * and its own choice of `space-y` — so the container is stated once here and the
 * spacing stays the caller's. Sections inside are `StaggerItem`s; `AdminHeader`
 * already is one, so a screen gets its title arriving for free.
 */
export function AdminScreen({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <Stagger step={SECTION_STAGGER} className={cn("px-8 pb-10 pt-6", className)}>
      {children}
    </Stagger>
  );
}

export function AdminHeader({ eyebrow, title, subtitle, action }: { eyebrow: string; title: string; subtitle: string; action?: ReactNode }) {
  const t = useT();
  const environment = usePlatform()?.environment;
  const badge = environment ? (ENV_BADGES[environment] ?? { labelKey: "admin.env.dev", tone: "text-ink-soft" }) : null;
  return (
    <StaggerItem as="header" className="flex flex-wrap items-start justify-between gap-4">
      <div className="space-y-1">
        <p className="font-mono-tech text-xs uppercase tracking-widest text-accent-debug">{eyebrow}</p>
        <h1 className="text-2xl font-semibold text-ink">{title}</h1>
        <p className="text-sm text-ink-soft">{subtitle}</p>
      </div>
      <div className="flex items-center gap-3">
        {badge && (
          // i18n-max: 8 — a pill beside the header's action slot; it must not wrap.
          <span className={cn("inline-flex items-center gap-1.5 whitespace-nowrap rounded-full border border-border px-2.5 py-1 text-xs font-medium", badge.tone)}>
            <span className="size-1.5 rounded-full bg-current" />
            {t(badge.labelKey)}
          </span>
        )}
        {action}
      </div>
    </StaggerItem>
  );
}

export function StatusDot({ status, label }: { status: string; label?: string }) {
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-sm font-medium", statusTone(status))}>
      <span className="size-1.5 rounded-full bg-current" />
      {label ?? status}
    </span>
  );
}

export function Toggle({ on, onChange, disabled, label }: { on: boolean; onChange: (next: boolean) => void; disabled?: boolean; label?: string }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!on)}
      className={cn(
        "relative inline-flex h-5 w-9 shrink-0 items-center rounded-full outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50",
        on ? "bg-primary" : "bg-muted",
      )}
    >
      <span className={cn("inline-block size-4 rounded-full bg-ink transition-transform", on ? "translate-x-4.5" : "translate-x-0.5")} />
    </button>
  );
}
