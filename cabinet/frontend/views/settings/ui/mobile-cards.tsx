"use client";

// The cards the mobile root screen stacks (Figma `cabinet/mobile/settings`): the profile
// tap target, the editable account rows, and the entries into Security, Notifications and
// sign-out. The two pushed screens they open — Sessions and Notifications — are the same
// sections the desktop rail shows, so they live beside it rather than here.

import { useLocale, useT } from "@evinvest/i18n/react";

import { BadgeCheck, Loader2, LogOut } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { type ReactNode, useState } from "react";

import { Skeleton } from "@evinvest/uikit";

import { cabinetPath } from "@/shared/config/base-path";
import type { Session } from "@/shared/contracts";
import { cn } from "@/shared/lib/cn";
import { csrfHeader } from "@/shared/lib/csrf-client";
import { CARD, Chevron, Hairline, InitialsAvatar, ListCard, ListCardTitle, Pill, Row, RowLabel, RowValue } from "@/shared/ui/list-card";
import { formatEmail, formatPhone } from "@/views/settings/lib/contact";
import { CURRENCIES, type Form, labelOf, LANGUAGES, optionsOf, TIMEZONES } from "@/views/settings/lib/form";
import { initialsOfName } from "@/views/settings/lib/format";
import { PhoneField, ThemedSelect } from "@/views/settings/ui/fields";

/** `card-ProfileSummary` — the tap target into the full profile. */
export function ProfileSummaryCard({ loading, name, email, verified }: { loading: boolean; name: string; email: string | null; verified: boolean }) {
  const t = useT();
  return (
    <Link
      href="/profile"
      className={cn(CARD, "flex items-center gap-3.5 py-3.5 pl-3.5 pr-4 outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring active:bg-ink/5")}
    >
      <InitialsAvatar initials={initialsOfName(name, email)} className="size-11.5 text-base" />
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        {loading ? (
          <Skeleton className="h-4 w-32" />
        ) : (
          <div className="flex min-w-0 items-center gap-2">
            <span className="truncate text-sm font-semibold text-ink">{name}</span>
            {/* i18n-max: 12 — a `shrink-0` Pill beside the truncated display name. */}
            {verified && <Pill icon={BadgeCheck}>{t("ui.verified")}</Pill>}
          </div>
        )}
        {loading ? <Skeleton className="h-3 w-40" /> : <span className="truncate text-xs text-ink-soft">{formatEmail(email) || t("auth.notSignedIn")}</span>}
      </div>
      <Chevron className="size-5" />
    </Link>
  );
}

/** `card-Account` — one tappable row per editable preference; tapping expands the editor in place. */
export function AccountRowsCard({
  loading,
  form,
  email,
  fieldErrors,
  onChange,
}: {
  loading: boolean;
  form: Form | null;
  email: string | null;
  fieldErrors: Record<string, string>;
  onChange: (key: keyof Form, value: string) => void;
}) {
  const t = useT();
  const [open, setOpen] = useState<keyof Form | null>(null);
  const ready = !loading && !!form;
  const toggle = (key: keyof Form) => setOpen((k) => (k === key ? null : key));
  // Resolved once per render, so the row's collapsed value and the open editor's menu
  // cannot disagree about what an option is called.
  const currencies = optionsOf(CURRENCIES, t);
  const timezones = optionsOf(TIMEZONES, t);

  return (
    <ListCard>
      {/* Email is the IdP's — displayed, never editable here. */}
      <Row>
        <span className="shrink-0 text-sm font-medium text-ink">{t("ui.email")}</span>
        {loading ? <Skeleton className="h-3.5 w-36" /> : <RowValue>{formatEmail(email) || "—"}</RowValue>}
      </Row>
      <Hairline />
      <ExpandableRow label={t("settings.phone")} value={form ? formatPhone(form.phone) : ""} loading={!ready} open={open === "phone"} onToggle={() => toggle("phone")}>
        {form && <PhoneField initial={form.phone} onChange={(v) => onChange("phone", v)} error={fieldErrors.phone} />}
      </ExpandableRow>
      <Hairline />
      <ExpandableRow label={t("lang.switch")} value={form ? labelOf(LANGUAGES, form.language) : ""} loading={!ready} open={open === "language"} onToggle={() => toggle("language")}>
        {form && <ThemedSelect value={form.language} onChange={(v) => onChange("language", v)} options={LANGUAGES} placeholder={t("settings.selectLanguage")} error={fieldErrors.language} />}
      </ExpandableRow>
      <Hairline />
      <ExpandableRow label={t("settings.baseCurrency")} value={form ? labelOf(currencies, form.base_currency) : ""} loading={!ready} open={open === "base_currency"} onToggle={() => toggle("base_currency")}>
        {form && <ThemedSelect value={form.base_currency} onChange={(v) => onChange("base_currency", v)} options={currencies} placeholder={t("settings.selectCurrency")} error={fieldErrors.base_currency} />}
      </ExpandableRow>
      <Hairline />
      <ExpandableRow label={t("settings.timeZone")} value={form ? labelOf(timezones, form.timezone) : ""} loading={!ready} open={open === "timezone"} onToggle={() => toggle("timezone")}>
        {form && <ThemedSelect value={form.timezone} onChange={(v) => onChange("timezone", v)} options={timezones} placeholder={t("settings.selectTimeZone")} error={fieldErrors.timezone} />}
      </ExpandableRow>
    </ListCard>
  );
}

function ExpandableRow({
  label,
  value,
  loading,
  open,
  onToggle,
  children,
}: {
  label: string;
  value: string;
  loading: boolean;
  open: boolean;
  onToggle: () => void;
  children: ReactNode;
}) {
  return (
    <div className="flex min-w-0 flex-col">
      <button
        type="button"
        onClick={onToggle}
        disabled={loading}
        aria-expanded={open}
        className="flex min-w-0 items-center justify-between gap-3 rounded-md py-3.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <span className="shrink-0 text-sm font-medium text-ink">{label}</span>
        <span className="flex min-w-0 items-center gap-2">
          {loading ? <Skeleton className="h-3.5 w-28" /> : !open && <RowValue>{value || "—"}</RowValue>}
          <Chevron className={cn("transition-transform", open && "rotate-90")} />
        </span>
      </button>
      {open && <div className="pb-3.5">{children}</div>}
    </div>
  );
}

/** `card-Security`, restated for the real auth model: Google-managed sign-in + live sessions. */
export function MobileSecurityCard({
  loading,
  email,
  sessions,
  onOpenSessions,
}: {
  loading: boolean;
  email: string | null;
  sessions: Session[] | undefined;
  onOpenSessions: () => void;
}) {
  const t = useT();
  return (
    <ListCard>
      <ListCardTitle>{t("ui.security")}</ListCardTitle>
      <Hairline />
      <Row>
        <RowLabel title={t("ui.signedInGoogle")} sub={loading ? "…" : formatEmail(email) || "—"} />
        {/* i18n-max: 12 — a `shrink-0` Pill beside the `min-w-0` row label. */}
        <Pill>{t("settings.connected")}</Pill>
      </Row>
      <Hairline />
      <button
        type="button"
        onClick={onOpenSessions}
        className="flex min-w-0 items-center justify-between gap-3 rounded-md py-3.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <RowLabel title={t("ui.trustedSessions")} sub={t("settings.trustedSessionsSub")} />
        <span className="flex shrink-0 items-center gap-2">
          {sessions === undefined ? <Skeleton className="h-3.5 w-4" /> : <RowValue>{sessions.length}</RowValue>}
          <Chevron />
        </span>
      </button>
      <Hairline />
      <p className="py-3 text-xs leading-relaxed text-ink-soft">{t("settings.googleManagedShort")}</p>
    </ListCard>
  );
}

/** `card-Notifications` — the entry into delivery preferences. The mock's four topic
 *  switches live on the pushed screen, where the real per-topic state comes from. */
export function MobileNotificationsCard({ onOpen }: { onOpen: () => void }) {
  const t = useT();
  return (
    <ListCard>
      <ListCardTitle>{t("nav.notifications")}</ListCardTitle>
      <Hairline />
      <button
        type="button"
        onClick={onOpen}
        className="flex min-w-0 items-center justify-between gap-3 rounded-md py-3.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <RowLabel title={t("ui.deliveryTopics")} sub={t("settings.deliveryTopicsSub")} />
        <Chevron />
      </button>
    </ListCard>
  );
}

/** `btn-SignOut` — the shell-owned logout, mirroring the header account chip. */
export function SignOutButton() {
  const locale = useLocale();
  const t = useT();
  const [busy, setBusy] = useState(false);
  async function signOut() {
    setBusy(true);
    // Site-root /api/auth: revokes the shared session and clears its cookies for every zone.
    await fetch("/api/auth/logout", { method: "POST", headers: csrfHeader() });
    window.location.href = cabinetPath(locale, "/loggedout");
  }
  // Kept hand-written: at 48px tall it sits between uikit's `sm` (32) and `lg` (40)
  // Button heights, so it carries its own focus ring rather than being resized.
  return (
    <button
      type="button"
      onClick={signOut}
      disabled={busy}
      className="flex w-full items-center justify-center gap-2 rounded-lg border border-border py-3.5 text-sm font-semibold text-accent-error outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring active:bg-accent-error/10 disabled:opacity-60"
    >
      {busy ? <Loader2 className="size-4 animate-spin" /> : <LogOut className="size-4" />} {t("auth.signOut")}
    </button>
  );
}
