"use client";

// The small pieces the two editors are built from: the desktop card header, a labelled
// field slot and its skeleton, the verified tag, and the two controls — select and phone —
// that the mobile row editors and the desktop form share, so a preference is edited the
// same way at both breakpoints.

import { useT } from "@evinvest/i18n/react";

import { BadgeCheck } from "lucide-react";
import { type ReactNode, useEffect, useRef } from "react";

import { PhoneNumber } from "@evinvest/types";
import { usePhoneNumber } from "@evinvest/types/react";
import { Input, Select, SelectContent, SelectItem, SelectTrigger, Skeleton } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";
import { labelOf } from "@/views/settings/lib/form";

/** Desktop card header. */
export function SectionHeader({ title, sub }: { title: string; sub: string }) {
  return (
    <header className="mb-4">
      <h2 className="text-sm font-semibold tracking-normal text-ink">{title}</h2>
      <p className="text-xs text-ink-soft">{sub}</p>
    </header>
  );
}

export function Field({ label, trailing, children }: { label: string; trailing?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex min-w-65 flex-1 flex-col">
      <div className="mb-1.5 flex items-center justify-between">
        <span className="text-xs text-ink-soft">{label}</span>
        {trailing}
      </div>
      {children}
    </div>
  );
}

export function FieldSkeleton() {
  return <Skeleton className="h-9 w-full rounded-md" />;
}

export function VerifiedTag() {
  const t = useT();
  return (
    <span className="inline-flex items-center gap-1 text-xs font-semibold text-accent-debug">
      {/* i18n-max: 12 — sits beside a field label in a `justify-between` header row. */}
      <BadgeCheck className="size-3" /> {t("ui.verified")}
    </span>
  );
}

export function ThemedSelect({
  value,
  onChange,
  options,
  placeholder,
  error,
}: {
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
  placeholder: string;
  error?: string;
}) {
  return (
    <div className="min-w-0 flex-1">
      <Select value={value || undefined} onValueChange={onChange}>
        <SelectTrigger className="w-full border-border bg-secondary">
          {/* Not `SelectValue`: the uikit's renders the raw stored value, so the trigger
              read "en" / "Asia/Ho_Chi_Minh" instead of the option label the design shows. */}
          <span className={cn("truncate", !value && "text-ink-soft")}>{value ? labelOf(options, value) : placeholder}</span>
        </SelectTrigger>
        <SelectContent>
          {options.map((o) => (
            <SelectItem key={o.value} value={o.value}>
              {o.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {error && <p className="mt-1 text-xs text-accent-error">{error}</p>}
    </div>
  );
}

/** Phone input wired to the `PhoneNumber` TypeObject via `usePhoneNumber`. */
export function PhoneField({ initial, onChange, error }: { initial: string; onChange: (v: string) => void; error?: string }) {
  const { inputProps, value, reset } = usePhoneNumber({ initial: initial || undefined });
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;

  // Propagate canonical value changes to the parent form state.
  useEffect(() => {
    onChangeRef.current(value ? PhoneNumber.raw(value) : "");
  }, [value]);

  // Sync external initial changes (e.g. after save reloads the profile).
  useEffect(() => {
    if (initial) reset(initial);
  }, [initial, reset]);

  return (
    <div className="min-w-0 flex-1">
      <Input {...inputProps} className={error ? "border-accent-error bg-accent-error/5" : "border-border bg-secondary"} />
      {error && <p className="mt-1 text-xs text-accent-error">{error}</p>}
    </div>
  );
}
