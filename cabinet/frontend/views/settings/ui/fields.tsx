"use client";

// The small pieces the two editors are built from: the desktop card header, a labelled
// field slot and its skeleton, the verified tag, and the two controls — select and phone —
// that the mobile row editors and the desktop form share, so a preference is edited the
// same way at both breakpoints.

import { useT } from "@evinvest/i18n/react";

import { BadgeCheck } from "lucide-react";
import { type ComponentProps, type ReactNode, createContext, useContext, useEffect, useId, useRef } from "react";

import { PhoneNumber } from "@evinvest/types";
import { usePhoneNumber } from "@evinvest/types/react";
import { Input, Select, SelectContent, SelectItem, SelectTrigger, Skeleton } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { Pill } from "@/shared/ui/list-card";
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

/** The id of the enclosing `Field`'s caption, so the control inside can name itself by it. */
const FieldLabelId = createContext<string | undefined>(undefined);

/** An `Input` named by the enclosing `Field`'s caption. */
export function FieldInput(props: ComponentProps<typeof Input>) {
  return <Input aria-labelledby={useContext(FieldLabelId)} {...props} />;
}

/**
 * A labelled field slot. `hint` is the one sentence under the control that says what the
 * value is used for — it stays visible, where a `tip` is the ⓘ beside the label for the
 * longer explanation a reader opens on demand. A field takes either, both, or neither.
 */
export function Field({ label, hint, tip, trailing, children }: { label: string; hint?: string; tip?: TipKey; trailing?: ReactNode; children: ReactNode }) {
  const labelId = useId();
  return (
    <div className="flex min-w-65 flex-1 flex-col">
      <div className="mb-1.5 flex items-center justify-between">
        <span className="flex items-center gap-1.5 text-xs text-ink-soft">
          {/* Its own span, so the control is named by the caption and not the ⓘ beside it. */}
          <span id={labelId}>{label}</span>
          {tip && <TipAnchor anchor={tip} />}
        </span>
        {trailing}
      </div>
      <FieldLabelId.Provider value={labelId}>{children}</FieldLabelId.Provider>
      {hint && <FieldHint>{hint}</FieldHint>}
    </div>
  );
}

/** The sentence under a control. Shared with the mobile row editors so both breakpoints word a field the same way. */
export function FieldHint({ children }: { children: ReactNode }) {
  return <p className="mt-1.5 text-xs leading-snug text-ink-soft">{children}</p>;
}

export function FieldSkeleton() {
  return <Skeleton className="h-9 w-full rounded-md" />;
}

export function VerifiedTag() {
  const t = useT();
  return (
    <span className="inline-flex items-center gap-1">
      {/* i18n-max: 12 — sits beside a field label in a `justify-between` header row. */}
      <Pill tone="success" icon={BadgeCheck}>
        {t("ui.verified", "Verified")}
      </Pill>
      {/* Verified means the address, not the person — the tip says so before anyone
          reads it as KYC. */}
      <TipAnchor anchor="profile.email.verified" />
    </span>
  );
}

export function ThemedSelect({
  value,
  onChange,
  options,
  placeholder,
  error,
  label,
}: {
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
  placeholder: string;
  error?: string;
  /** Accessible name where no `Field` caption encloses the select (the mobile row editors). */
  label?: string;
}) {
  const labelId = useContext(FieldLabelId);
  return (
    <div className="min-w-0 flex-1">
      <Select value={value || undefined} onValueChange={onChange}>
        <SelectTrigger className="w-full border-border bg-secondary" aria-labelledby={labelId} aria-label={labelId ? undefined : label}>
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
      <FieldInput {...inputProps} className={error ? "border-accent-error bg-accent-error/5" : "border-border bg-secondary"} />
      {error && <p className="mt-1 text-xs text-accent-error">{error}</p>}
    </div>
  );
}
