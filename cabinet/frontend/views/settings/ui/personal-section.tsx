"use client";

// Personal details: the identity fields the fund records — the seven the old profile
// editor held, plus the read-only email. Two shapes from one field list: the desktop pane
// (labelled fields in a card) and the mobile pushed screen (label-over-input rows). The
// hint under each control is the same at both breakpoints, so a field is explained once.

import { useT } from "@evinvest/i18n/react";

import { Skeleton } from "@evinvest/uikit";

import { cn } from "@/shared/lib/cn";
import { TipAnchor } from "@/shared/tips";
import { CARD, Hairline, ListCard, StackRow } from "@/shared/ui/list-card";
import { formatEmail } from "@/views/settings/lib/contact";
import type { Form } from "@/views/settings/lib/form";
import { PERSONAL } from "@/views/settings/lib/sections";
import { Field, FieldHint, FieldInput, FieldSkeleton, PhoneField, SectionHeader, VerifiedTag } from "@/views/settings/ui/fields";

export interface PersonalProps {
  loading: boolean;
  form: Form | null;
  email: string | null;
  verified: boolean;
  onChange: (key: keyof Form, value: string) => void;
  fieldErrors: Record<string, string>;
}

/** The desktop pane. */
export function PersonalSection({ loading, form, email, verified, onChange, fieldErrors }: PersonalProps) {
  const t = useT();
  const ready = !loading && !!form;
  return (
    <section className={cn(CARD, "px-6 py-5.5")}>
      <SectionHeader title={t("settings.nav.personal", "Personal details")} sub={t("settings.personalSub", "Your identity as the fund records it")} />
      {/* A section-type tip — a descriptor block, not an inline ⓘ — so it sits under the
          header rather than in it. */}
      <TipAnchor anchor="profile.personal.compliance" className="mb-4" />
      <div className="flex flex-wrap gap-x-4.5 gap-y-4">
        {PERSONAL.map((field) => (
          <Field key={field.key} label={field.label(t)} hint={field.hint?.(t)} tip={field.tip}>
            {ready ? <Control field={field.key} form={form} error={fieldErrors[field.key]} onChange={onChange} /> : <FieldSkeleton />}
          </Field>
        ))}
        <Field label={t("ui.emailAddress", "Email address")} hint={t("settings.hint.email", "Comes from your Google account and can't be changed here.")} trailing={verified ? <VerifiedTag /> : undefined}>
          {loading ? <FieldSkeleton /> : <FieldInput value={formatEmail(email)} readOnly className="border-border bg-secondary text-ink-soft" />}
        </Field>
      </div>
    </section>
  );
}

/** The mobile pushed screen — the same fields stacked label over input, hairlines between. */
export function PersonalStack({ loading, form, email, verified, onChange, fieldErrors }: PersonalProps) {
  const t = useT();
  const ready = !loading && !!form;
  return (
    <div className="flex flex-col gap-4">
      <TipAnchor anchor="profile.personal.compliance" />
      <ListCard>
        {PERSONAL.map((field, i) => (
          <div key={field.key}>
            {i > 0 && <Hairline />}
            <StackRow
              label={
                <span className="flex items-center gap-1.5">
                  {field.label(t)}
                  {field.tip && <TipAnchor anchor={field.tip} />}
                </span>
              }
            >
              {ready ? <Control field={field.key} form={form} error={fieldErrors[field.key]} onChange={onChange} /> : <Skeleton className="h-9 w-full rounded-md" />}
              {field.hint && <FieldHint>{field.hint(t)}</FieldHint>}
            </StackRow>
          </div>
        ))}
        <Hairline />
        <StackRow
          label={
            <span className="flex items-center justify-between gap-2">
              {t("ui.emailAddress", "Email address")}
              {verified && <VerifiedTag />}
            </span>
          }
        >
          {loading ? <Skeleton className="h-5 w-48" /> : <span className="break-words text-sm font-medium text-ink-soft">{formatEmail(email) || "—"}</span>}
          <FieldHint>{t("settings.hint.email", "Comes from your Google account and can't be changed here.")}</FieldHint>
        </StackRow>
      </ListCard>
    </div>
  );
}

// One control per field, error under it — the phone goes through the `PhoneNumber`
// TypeObject so the stored value is canonical, everything else is a plain text input.
function Control({ field, form, error, onChange }: { field: keyof Form; form: Form; error?: string; onChange: (key: keyof Form, value: string) => void }) {
  if (field === "phone") return <PhoneField initial={form.phone} onChange={(v) => onChange("phone", v)} error={error} />;
  return (
    <div className="min-w-0 flex-1">
      <FieldInput value={form[field]} onChange={(e) => onChange(field, e.target.value)} className={error ? "border-accent-error bg-accent-error/5" : "border-border bg-secondary"} />
      {error && <p className="mt-1 text-xs text-accent-error">{error}</p>}
    </div>
  );
}
