"use client";

import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";

import { roleLabel, statusLabel } from "@/views/admin/lib/format";

import { BadgeCheck, Loader2 } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";

import { PhoneNumber } from "@evinvest/types";
import { usePhoneNumber } from "@evinvest/types/react";
import { Button, Input, Skeleton } from "@evinvest/uikit";

import { positionsResource } from "@/entities/fund/model/fund-resource";
import { StartVerificationRow } from "@/features/kyc";
import { profileResource, saveProfile } from "@/entities/user/model/profile-resource";
import { validateProfileForm } from "@/entities/user/model/profile-schema";
import type { UpdateProfileRequest, UserProfile } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { formatUsd, num } from "@/shared/lib/money";
import { useResource } from "@/shared/lib/resource";
import { CARD, Hairline, InitialsAvatar, ListCard, ListCardTitle, Pill, type PillTone, Row, RowLabel, RowValue, StackRow } from "@/shared/ui/list-card";
import { MobileAppBar } from "@/shared/ui/mobile-appbar";
import { SECTION_STAGGER, Stagger, StaggerItem } from "@/shared/ui/motion";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { displayName, initialsOfName, truncateName } from "@/views/profile/lib/format";

// The investor profile. Two Figma frames, one component: `cabinet/mobile/profile`
// (node 503:266) below `lg` — a hero card over stacked label/value cards — and
// `cabinet/profile` (node 489:258) above it, a two-column form beside the verification
// and snapshot cards.
//
// Name/email and the editable personal fields are real (core UsersService via the BFF),
// as are Identity verification (the admin-managed `status`/`kyc_level`/`email_verified`
// on the user record) and the Account snapshot (fund positions). The mock's per-document
// KYC rows (passport, proof of address, source of funds) have no document store behind
// them, so this states the KYC level the hub actually holds instead of inventing four.

// All editable fields are held in the form (initialised from the loaded profile) even
// though this surface only shows seven of them — UpdateProfile is full-replace, so
// carrying language/currency/timezone preserves what Settings → General owns.
const EDITABLE = ["legal_name", "preferred_name", "phone", "date_of_birth", "nationality", "tax_residence", "residential_address", "language", "base_currency", "timezone"] as const;
type Form = Record<(typeof EDITABLE)[number], string>;

// Module scope, so the labels are catalogue keys rather than finished English; both
// breakpoints resolve them at render. `tip` is a TipKey (a compile-time id), not a label.
const SHOWN: { key: (typeof EDITABLE)[number]; label: (t: Translate) => string; tip?: TipKey }[] = [
  { key: "legal_name", label: t => t("profile.legalName", "Legal name"), tip: "profile.field.legal-name" },
  { key: "preferred_name", label: t => t("profile.preferredName", "Preferred name") },
  { key: "phone", label: t => t("profile.phoneNumber", "Phone number") },
  { key: "date_of_birth", label: t => t("profile.dateOfBirth", "Date of birth") },
  { key: "nationality", label: t => t("profile.nationality", "Nationality"), tip: "profile.field.nationality" },
  { key: "tax_residence", label: t => t("profile.taxResidence", "Tax residence"), tip: "profile.field.tax-residence" },
  { key: "residential_address", label: t => t("profile.residentialAddress", "Residential address") },
];

function formFrom(p: UserProfile): Form {
  return Object.fromEntries(EDITABLE.map((k) => [k, p[k] ?? ""])) as Form;
}

export function ProfileView() {
  const t = useT();
  const [form, setForm] = useState<Form | null>(null);
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});

  // Both reads are cached: the profile with the account chip and Settings, the positions
  // with Home and Invest. Arriving from any of them fills this page on the first frame.
  const { data: profile, error: readError, isLoading: loading } = useResource(profileResource);
  const positions = useResource(positionsResource).data?.positions ?? [];
  const error = saveError ?? (profile || !readError ? null : errorMessage(readError, t));

  // Seed the form during render, not in an effect, so a cached profile fills the fields on
  // the frame it is read rather than one frame later. Never while the user is editing: a
  // background refresh must not overwrite what they are part-way through typing.
  const [seeded, setSeeded] = useState<UserProfile | null>(null);
  if (profile && profile !== seeded && !editing) {
    setSeeded(profile);
    setForm(formFrom(profile));
  }

  const email = profile?.email ?? "";
  const legalName = (profile?.legal_name ?? "").trim();
  const name = truncateName(legalName) || (loading ? "…" : displayName(email, t));
  const invested = positions.reduce((s, p) => s + num(p.value), 0);

  function set(key: keyof Form, value: string) {
    setForm((f) => (f ? { ...f, [key]: value } : f));
  }
  function cancel() {
    if (profile) setForm(formFrom(profile));
    setEditing(false);
    setSaveError(null);
    setFieldErrors({});
  }
  async function save() {
    if (!form) return;
    const errors = validateProfileForm(form, t);
    if (Object.keys(errors).length > 0) {
      setFieldErrors(errors);
      setSaveError(t("settings.fixHighlighted", "Please fix the highlighted fields"));
      return;
    }
    setFieldErrors({});
    setSaving(true);
    setSaveError(null);
    try {
      // `saveProfile` publishes the PATCH response into the cache, so this page, the
      // sidebar account chip and Settings all show the new name without a refetch.
      const updated = await saveProfile(form as UpdateProfileRequest);
      setForm(formFrom(updated));
      setEditing(false);
    } catch (e) {
      setSaveError(errorMessage(e, t));
    } finally {
      setSaving(false);
    }
  }

  const verification = <VerificationCard loading={loading} profile={profile ?? null} email={email} />;
  const snapshot = <SnapshotCard invested={invested} strategies={positions.length} />;

  return (
    <>
      <MobileAppBar title={t("ui.profile", "Profile")} backHref="/settings" />

      <Stagger delay={SECTION_STAGGER} step={SECTION_STAGGER} className="flex flex-col gap-4 px-5 pb-6 pt-4.5 lg:gap-5 lg:px-8 lg:pb-8 lg:pt-6">
        {/* Desktop page heading — the mobile app bar owns this below `lg`. */}
        <StaggerItem className="hidden items-center justify-between gap-4 lg:flex">
          <div className="min-w-0">
            <h1 className="text-2xl font-semibold text-ink">{t("ui.profile", "Profile")}</h1>
            <p className="text-sm text-ink-soft">{t("profile.subtitle", "Your personal details and verification status")}</p>
          </div>
          {/* `shrink-0` Buttons beside a `min-w-0` heading column — their width is taken
              out of the page title, so the labels stay short. */}
          {/* i18n-max: 11 for the two verbs, 20 for the edit CTA. */}
          <div className="flex shrink-0 gap-2">
            {editing ? (
              <>
                <Button variant="outline" size="sm" className="border-border" onClick={cancel} disabled={saving}>
                  {t("ui.cancel", "Cancel")}
                </Button>
                <Button type="button" onClick={save} disabled={saving} className="rounded-lg font-semibold">
                  {saving && <Loader2 className="size-4 animate-spin" />} {t("ui.save", "Save")}
                </Button>
              </>
            ) : (
              <Button type="button" onClick={() => setEditing(true)} disabled={loading || profile === null} className="rounded-lg font-semibold">
                {t("profile.editProfile", "Edit profile")}
              </Button>
            )}
          </div>
        </StaggerItem>

        {error && (
          <StaggerItem as="p" className="rounded-md border border-accent-error/40 bg-accent-error/10 px-3 py-2 text-sm text-accent-error">
            {error}
          </StaggerItem>
        )}

        {/* card-Hero — centred on mobile (Figma 503:274), a wide chip on desktop (489:258). */}
        <StaggerItem className={cn(CARD, "flex flex-col items-center gap-3 px-5 pb-5.5 pt-6 lg:flex-row lg:gap-5 lg:px-6 lg:py-5.5")}>
          <InitialsAvatar initials={initialsOfName(name, email)} className="size-16 text-2xl lg:text-xl" />
          <div className="flex min-w-0 flex-1 flex-col items-center gap-1 text-center lg:items-start lg:text-left">
            <div className="flex min-w-0 flex-col items-center gap-1 lg:flex-row lg:items-baseline lg:gap-3">
              {loading ? <Skeleton className="h-6 w-40" /> : <p className="truncate text-lg font-semibold text-ink lg:text-xl">{name || t("ui.account", "Account")}</p>}
              {loading ? <Skeleton className="h-4 w-48" /> : <p className="truncate text-sm text-ink-soft">{email || t("auth.notSignedIn", "Not signed in")}</p>}
            </div>
            {!loading && (
              // i18n-max: 12 per Pill — they sit beside the truncated display name.
              <div className="mt-1 flex flex-wrap items-center justify-center gap-2 lg:justify-start">
                {profile?.email_verified && <Pill icon={BadgeCheck}>{t("ui.verified", "Verified")}</Pill>}
                {profile?.status && <Pill tone={statusTone(profile.status)}>{statusLabel(profile.status, t)}</Pill>}
                {profile?.kyc_level !== undefined && <Pill tone="neutral">{t("profile.kycLevelPill", "KYC level {n}", { n: profile.kyc_level })}</Pill>}
              </div>
            )}
          </div>
          {/* Mobile puts the primary action in the hero; desktop has it in the page heading. */}
          <div className="flex w-full gap-2 lg:hidden">
            {editing ? (
              <>
                <Button variant="outline" className="h-9 flex-1 border-input" onClick={cancel} disabled={saving}>
                  {t("ui.cancel", "Cancel")}
                </Button>
                <Button type="button" onClick={save} disabled={saving} className="h-9 flex-1 gap-1.5 font-semibold">
                  {saving && <Loader2 className="size-4 animate-spin" />} {t("ui.save", "Save")}
                </Button>
              </>
            ) : (
              <Button variant="outline" className="h-9 w-full border-input" onClick={() => setEditing(true)} disabled={loading || profile === null}>
                {t("profile.editProfile", "Edit profile")}
              </Button>
            )}
          </div>
        </StaggerItem>

        {/* ── Mobile (Figma cabinet/mobile/profile) ────────────────────────── */}
        {/* One item per viewport block rather than per card: `verification` and
            `snapshot` are the same two elements rendered into both, and giving each a
            place in the sequence would have them animating twice over. */}
        <StaggerItem className="flex flex-col gap-4 lg:hidden">
          <ListCard>
            <ListCardTitle
              sub={profile?.role ? <span className="text-accent-debug/85">{t("profile.roleAccount", "{role} account", { role: roleLabel(profile.role, t) })}</span> : undefined}
            >
              {t("profile.personalInformation", "Personal information")}
            </ListCardTitle>
            {SHOWN.map(({ key, label }, i) => (
              <div key={key}>
                {i > 0 && <Hairline />}
                <StackRow label={label(t)}>
                  {loading || !form ? (
                    <Skeleton className="h-5 w-40" />
                  ) : editing ? (
                    key === "phone" ? (
                      <PhoneField initial={form.phone} onChange={(v) => set("phone", v)} error={fieldErrors.phone} />
                    ) : (
                      <div className="min-w-0">
                        <Input
                          value={form[key]}
                          onChange={(e) => set(key, e.target.value)}
                          className={fieldErrors[key] ? "border-accent-error bg-accent-error/5" : "border-border bg-secondary"}
                        />
                        {fieldErrors[key] && <p className="mt-1 text-xs text-accent-error">{fieldErrors[key]}</p>}
                      </div>
                    )
                  ) : (
                    <span className={cn("break-words text-sm font-medium", profile?.[key] ? "text-ink" : "text-ink-soft")}>
                      {(key === "phone" ? formatPhone(profile?.[key]) : profile?.[key]) || "—"}
                    </span>
                  )}
                </StackRow>
              </div>
            ))}
            <Hairline />
            <StackRow label={t("ui.emailAddress", "Email address")}>
              {loading ? <Skeleton className="h-5 w-48" /> : <span className="break-words text-sm font-medium text-ink-soft">{email || "—"}</span>}
            </StackRow>
          </ListCard>

          {verification}
          {snapshot}
        </StaggerItem>

        {/* ── Desktop (Figma cabinet/profile) ──────────────────────────────── */}
        <StaggerItem className="hidden items-start gap-5 lg:flex">
          <section className={cn(CARD, "w-full flex-1 space-y-4.5 px-6 py-5.5")}>
            <header>
              <h2 className="text-sm font-semibold tracking-normal text-ink">{t("profile.personalInformation", "Personal information")}</h2>
              <p className="text-xs text-ink-soft">{t("profile.personalInformationSub", "Used for compliance and statements")}</p>
            </header>
            <div className="flex flex-wrap gap-x-4.5 gap-y-4">
              {SHOWN.map(({ key, label, tip }) => (
                <FieldBox key={key} label={label(t)} tip={tip}>
                  {loading || !form ? (
                    <Skeleton className="h-10.5 w-full rounded-lg" />
                  ) : editing ? (
                    key === "phone" ? (
                      <PhoneField initial={form.phone} onChange={(v) => set("phone", v)} error={fieldErrors.phone} />
                    ) : (
                      <div className="min-w-0 flex-1">
                        <Input
                          value={form[key]}
                          onChange={(e) => set(key, e.target.value)}
                          className={fieldErrors[key] ? "border-accent-error bg-accent-error/5" : "border-border bg-secondary"}
                        />
                        {fieldErrors[key] && <p className="mt-1 text-xs text-accent-error">{fieldErrors[key]}</p>}
                      </div>
                    )
                  ) : (
                    <ReadValue value={key === "phone" ? formatPhone(profile?.[key]) : profile?.[key]} />
                  )}
                </FieldBox>
              ))}
              <FieldBox label={t("ui.emailAddress", "Email address")} trailing={profile?.email_verified ? <VerifiedTag /> : undefined}>
                {loading ? <Skeleton className="h-10.5 w-full rounded-lg" /> : <ReadValue value={email} muted />}
              </FieldBox>
            </div>
          </section>

          <div className="flex w-97 shrink-0 flex-col gap-5">
            {verification}
            {snapshot}
          </div>
        </StaggerItem>
      </Stagger>
    </>
  );
}

/** `card-Verification` — the KYC state the hub actually holds, not a document checklist. */
function VerificationCard({ loading, profile, email }: { loading: boolean; profile: UserProfile | null; email: string }) {
  const t = useT();
  return (
    <ListCard className="lg:px-5.5">
      <ListCardTitle sub={t("profile.identityAndStanding", "Identity and account standing")}>{t("profile.identityVerification", "Identity verification")}</ListCardTitle>
      <Hairline />
      <Row>
        <RowLabel title={t("ui.emailAddress", "Email address")} sub={loading ? "…" : email || "—"} />
        {/* i18n-max: 12 — `shrink-0` Pills beside the `min-w-0` row label. */}
        {loading ? <Skeleton className="h-5 w-16 rounded-full" /> : profile?.email_verified ? <Pill icon={BadgeCheck}>{t("ui.verified", "Verified")}</Pill> : <Pill tone="pending">{t("profile.unverified", "Unverified")}</Pill>}
      </Row>
      <Hairline />
      <Row>
        <RowLabel title={t("ui.kycLevel", "KYC level")} sub={t("profile.kycRaisedBy", "Raised by verification and review")} />
        {loading ? <Skeleton className="h-4 w-10" /> : <RowValue className="font-semibold tabular-nums text-ink">{profile?.kyc_level ?? "—"}</RowValue>}
      </Row>
      <Hairline />
      <Row>
        <RowLabel title={t("ui.accountStatus", "Account status")} sub={t("profile.platformAccess", "Platform access")} />
        {loading ? <Skeleton className="h-5 w-16 rounded-full" /> : profile?.status ? <Pill tone={statusTone(profile.status)}>{statusLabel(profile.status, t)}</Pill> : <RowValue>—</RowValue>}
      </Row>
      {canStartVerification(profile) && <StartVerificationRow />}
    </ListCard>
  );
}

/**
 * Only the entry tier is self-serve: `/kyc/start` opens a case for tier 1 and nothing
 * above it, so offering this to someone already past 1 would spend a paid vendor session
 * on a verdict that, by the plane's own rule, cannot raise them. Higher tiers stay a
 * compliance action.
 *
 * An absent `kyc_level` counts as 0 rather than unknown — proto3 omits zero-valued
 * scalars from JSON, so "no field" is exactly how an unverified user arrives.
 */
function canStartVerification(profile: UserProfile | null): boolean {
  return profile !== null && (profile.kyc_level ?? 0) === 0;
}

function SnapshotCard({ invested, strategies }: { invested: number; strategies: number }) {
  const t = useT();
  return (
    <ListCard className="lg:px-5.5">
      <ListCardTitle>{t("profile.accountSnapshot", "Account snapshot")}</ListCardTitle>
      <Hairline />
      <Row>
        <span className="text-sm font-medium text-ink-soft">{t("profile.totalInvested", "Total invested")}</span>
        <span className="text-sm font-semibold tabular-nums text-ink">{formatUsd(invested)}</span>
      </Row>
      <Hairline />
      <Row>
        <span className="text-sm font-medium text-ink-soft">{t("dash.activeStrategies", "Active strategies")}</span>
        <span className="text-sm font-semibold tabular-nums text-ink">{strategies}</span>
      </Row>
    </ListCard>
  );
}

function FieldBox({ label, trailing, tip, children }: { label: string; trailing?: ReactNode; tip?: TipKey; children: ReactNode }) {
  return (
    <div className="flex min-w-65 flex-1 flex-col">
      <div className="mb-1.5 flex items-center justify-between">
        <span className="flex items-center gap-1.5 text-xs text-ink-soft">
          {label}
          {tip && <TipAnchor anchor={tip} />}
        </span>
        {trailing}
      </div>
      {children}
    </div>
  );
}

const MAX_DISPLAY_VALUE = 80;

function ReadValue({ value, muted }: { value?: string; muted?: boolean }) {
  const raw = (value ?? "").trim();
  const v = raw.length > MAX_DISPLAY_VALUE ? `${raw.slice(0, MAX_DISPLAY_VALUE - 1)}…` : raw;
  return (
    <div
      className={cn(
        "flex min-h-10.5 items-center rounded-lg border border-border bg-secondary px-3.5 py-3 text-sm",
        v && !muted ? "text-ink" : "text-ink-soft",
      )}
      title={raw.length > MAX_DISPLAY_VALUE ? raw : undefined}
    >
      {v || "—"}
    </div>
  );
}

function VerifiedTag() {
  const t = useT();
  return (
    <span className="inline-flex items-center gap-1 text-xs font-semibold text-accent-debug">
      {/* i18n-max: 12 — beside a field label in a `justify-between` header row. */}
      <BadgeCheck className="size-3" /> {t("ui.verified", "Verified")}
      <TipAnchor anchor="profile.email.verified" />
    </span>
  );
}


function statusTone(status: string): PillTone {
  const s = status.toLowerCase();
  if (s === "active") return "positive";
  if (s === "pending" || s === "review") return "pending";
  return "neutral";
}

function formatPhone(raw?: string): string {
  if (!raw) return "";
  const pn = PhoneNumber.parseInput(raw) ?? PhoneNumber.fromUnsafe(raw);
  return PhoneNumber.format(pn);
}

/** Phone input wired to the `PhoneNumber` TypeObject via `usePhoneNumber`. */
function PhoneField({ initial, onChange, error }: { initial: string; onChange: (v: string) => void; error?: string }) {
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
