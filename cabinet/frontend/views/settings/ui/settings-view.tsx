"use client";

import { Bell, Check, Loader2, type LucideIcon, Monitor, Shield, User } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { Button } from "@evinvest/uikit";

import { revokeSession, sessionsResource } from "@/entities/session/model/session-resource";
import { isLocale } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";

import { relocalise } from "@/shared/config/base-path";
import { writeLocaleCookie } from "@/shared/lib/locale-cookie";
import { profileResource, saveProfile } from "@/entities/user/model/profile-resource";
import { validateProfileForm } from "@/entities/user/model/profile-schema";
import type { UpdateProfileRequest, UserProfile } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { InitialsAvatar } from "@/shared/ui/list-card";
import { MobileAppBar } from "@/shared/ui/mobile-appbar";
import { Reveal, SECTION_STAGGER, Stagger, StaggerItem } from "@/shared/ui/motion";
import { EDITABLE, type Form, formFrom } from "@/views/settings/lib/form";
import { displayName, initialsOfName, truncateName } from "@/views/settings/lib/format";
import { GeneralSection } from "@/views/settings/ui/general-section";
import { AccountRowsCard, MobileNotificationsCard, MobileSecurityCard, ProfileSummaryCard, SignOutButton } from "@/views/settings/ui/mobile-cards";
import { NotificationsSection } from "@/views/settings/ui/notifications-section";
import { SecuritySection } from "@/views/settings/ui/security-section";
import { SessionsSection } from "@/views/settings/ui/sessions-section";

// The investor settings surface, wired to the backend over the BFF. Two Figma frames,
// one component: `cabinet/mobile/settings` (node 498:259) below `lg` — an app bar over a
// stack of row cards, with Sessions pushed as its own screen — and `cabinet/settings`
// (node 481:250) above it, a section rail beside the editing form.
//
// General edits the same core user record as the Profile page (full-replace, so the form
// carries every editable field even where a breakpoint only shows some); Security states
// the real auth model (Google-managed) and surfaces live sessions; Sessions & devices
// lists and revokes the real refresh-token families at the hub; Notifications is the real
// delivery-preference store. Auth is Google-OAuth-only and there is no theme store, so the
// mock's 2FA/biometric/password rows and its Preferences card have no backing here — they
// are left out rather than faked.

type Section = "general" | "security" | "sessions" | "notifications";

// Module-scope, so the labels are catalogue keys rather than finished English — the rail
// resolves them at render.
const NAV: { id: Section; labelKey: string; icon: LucideIcon }[] = [
  { id: "general", labelKey: "settings.nav.general", icon: User },
  { id: "security", labelKey: "ui.security", icon: Shield },
  { id: "sessions", labelKey: "ui.sessionsDevices", icon: Monitor },
  { id: "notifications", labelKey: "nav.notifications", icon: Bell },
];

export function SettingsView() {
  const locale = useLocale();
  const t = useT();
  const [section, setSection] = useState<Section>("general");
  // The mobile stack: the root screen, or a section pushed on top of it.
  const [pushed, setPushed] = useState<"sessions" | "notifications" | null>(null);

  const [form, setForm] = useState<Form | null>(null);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const savedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const [revokeError, setRevokeError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // The profile is the same cached read the account chip and the Profile page use; the
  // session list is its own, refreshed by the revoke below rather than by a manual reload.
  const { data: profile, error: profileError, isLoading: loading } = useResource(profileResource);
  const sessionList = useResource(sessionsResource);
  const sessions = sessionList.data;
  const error = saveError ?? (profile || !profileError ? null : errorMessage(profileError, t));
  const sessionsError = revokeError ?? (sessions || !sessionList.error ? null : errorMessage(sessionList.error, t));

  // Seeded during render, not in an effect, so a cached profile fills the form on the frame
  // it is read. Held back while the form is dirty: a background refresh must not overwrite
  // edits in progress.
  const [seeded, setSeeded] = useState<UserProfile | null>(null);
  const pristine = !form || !seeded || EDITABLE.every((k) => form[k] === (seeded[k] ?? ""));
  if (profile && profile !== seeded && pristine) {
    setSeeded(profile);
    setForm(formFrom(profile));
  }

  const email = profile?.email ?? null;
  const name = truncateName((profile?.legal_name ?? "").trim()) || displayName(email, t);
  const dirty = !pristine;

  function set(key: keyof Form, value: string) {
    setSaved(false);
    setForm((f) => (f ? { ...f, [key]: value } : f));
  }
  async function save() {
    if (!form || saving) return;
    const errors = validateProfileForm(form, t);
    if (Object.keys(errors).length > 0) {
      setFieldErrors(errors);
      setSaveError(t("settings.fixHighlighted"));
      return;
    }
    setFieldErrors({});
    setSaving(true);
    setSaveError(null);
    try {
      // Published into the cache by `saveProfile`, so the sidebar account chip and the
      // Profile page pick up the new name without a refetch.
      const updated = await saveProfile(form as UpdateProfileRequest);
      setForm(formFrom(updated));
      setSaved(true);
      if (savedTimer.current) clearTimeout(savedTimer.current);
      savedTimer.current = setTimeout(() => setSaved(false), 2500);
      // Language is the one field that changes the page it was edited on. It was
      // previously stored and nothing more: the value round-tripped to the profile and
      // the interface stayed in whatever language it was already in, so the control
      // looked broken even though it worked. Applying it is two steps, and both are
      // needed — the cookie so every later entry (a bookmark, the conductor's chip, an
      // unprefixed /cabinet link) resolves to the new choice, and the navigation so the
      // page the reader is looking at is actually re-rendered from the new catalogue.
      //
      // Server-confirmed value, not the form's: if the backend normalised or rejected
      // the code, the URL must follow what was actually stored rather than what was
      // typed. `en-US` and friends are stored fine but are not routable locales, so a
      // non-locale value simply leaves the interface where it is.
      const chosen = updated.language;
      if (isLocale(chosen) && chosen !== locale) {
        writeLocaleCookie(chosen);
        // A hard navigation, not router.push: the locale lives in the root layout's
        // segment, and the catalogue is chosen there at render time. `relocalise` is
        // shared with LocaleSync so the two cannot disagree about what "the same page"
        // means — it keeps the query and hash, which a hand-rolled version dropped.
        window.location.href = relocalise(chosen, window.location);
        return;
      }
    } catch (e) {
      setSaveError(errorMessage(e, t));
    } finally {
      setSaving(false);
    }
  }
  useEffect(
    () => () => {
      if (savedTimer.current) clearTimeout(savedTimer.current);
    },
    [],
  );

  async function revoke(id: string) {
    setBusy(true);
    setRevokeError(null);
    try {
      // The revoke invalidates the session list, so it refreshes itself.
      await revokeSession(id);
    } catch (e) {
      setRevokeError(errorMessage(e, t));
    } finally {
      setBusy(false);
    }
  }
  async function revokeOthers() {
    const others = (sessions ?? []).filter((s) => !s.current && s.id);
    if (!others.length) return;
    setBusy(true);
    setRevokeError(null);
    try {
      for (const s of others) await revokeSession(s.id!);
    } catch (e) {
      setRevokeError(errorMessage(e, t));
    } finally {
      setBusy(false);
    }
  }

  const sessionsPanel = (titled: boolean) => (
    <SessionsSection titled={titled} sessions={sessions} error={sessionsError} busy={busy} name={name} onRevoke={revoke} onRevokeOthers={revokeOthers} />
  );

  return (
    <>
      <MobileAppBar
        title={t(pushed === "sessions" ? "ui.sessionsDevices" : pushed === "notifications" ? "nav.notifications" : "nav.settings")}
        onBack={pushed ? () => setPushed(null) : undefined}
        right={
          dirty ? (
            // i18n-max: 11 — a `shrink-0` Button in the app bar, beside the truncated title.
            <Button type="button" size="sm" onClick={save} disabled={saving} className="rounded-full font-semibold">
              {saving && <Loader2 className="size-3.5 animate-spin" />} {t("ui.save")}
            </Button>
          ) : pushed ? undefined : (
            <InitialsAvatar initials={initialsOfName(name, email)} className="size-8.5 text-sm" />
          )
        }
      />

      <Stagger delay={SECTION_STAGGER} step={SECTION_STAGGER} className="flex flex-col gap-4 px-5 pb-6 pt-4.5 lg:gap-6 lg:px-8 lg:pb-10 lg:pt-6">
        {/* Desktop page heading — the mobile app bar owns this below `lg`. */}
        <StaggerItem className="hidden items-center justify-between gap-4 lg:flex">
          <div className="min-w-0">
            <h1 className="text-2xl font-semibold text-ink">{t("nav.settings")}</h1>
            <p className="text-sm text-ink-soft">{t("settings.subtitle")}</p>
          </div>
          {section === "general" && (
            // Both children are `shrink-0` beside a `min-w-0` heading column, so their
            // combined width comes straight out of the page title.
            <div className="flex shrink-0 items-center gap-3">
              {saved && (
                // i18n-max: 11
                <span className="inline-flex items-center gap-1 text-sm font-medium text-positive">
                  <Check className="size-4" /> {t("ui.saved")}
                </span>
              )}
              {/* i18n-max: 20 */}
              <Button type="button" onClick={save} disabled={loading || saving || !dirty} className="rounded-lg font-semibold">
                {saving && <Loader2 className="size-4 animate-spin" />} {t("ui.saveChanges")}
              </Button>
            </div>
          )}
        </StaggerItem>

        {error && (
          <StaggerItem as="p" className="rounded-md border border-accent-error/40 bg-accent-error/10 px-3 py-2 text-sm text-accent-error">
            {error}
          </StaggerItem>
        )}
        {/* Not a section: "Saved" appears in answer to a click, long after the page
            arrived, and belongs to the save rather than to the screen. */}
        {saved && (
          <p className="inline-flex items-center gap-1 text-sm font-medium text-positive lg:hidden">
            <Check className="size-4" /> {t("ui.saved")}
          </p>
        )}

        {/* ── Mobile (Figma cabinet/mobile/settings) ───────────────────────── */}
        {/* Pushing into Sessions or Notifications replaces the whole stack, so the `key`
            remounts the reveal and the new screen arrives instead of appearing. It
            repeats the column because a wrapper that did not would collapse the gap
            between the five root cards. On the page's own first paint this reveal is
            nested inside the entrance above it and fades without travelling — one
            movement, not two (see shared/ui/motion/entrance). */}
        <StaggerItem className="lg:hidden">
          <Reveal key={pushed ?? "root"} className="flex flex-col gap-4">
            {pushed === "sessions" ? (
              sessionsPanel(false)
            ) : pushed === "notifications" ? (
              <NotificationsSection />
            ) : (
              <>
                <ProfileSummaryCard loading={loading} name={name} email={email} verified={!!profile?.email_verified} />
                <AccountRowsCard loading={loading} form={form} email={email} fieldErrors={fieldErrors} onChange={set} />
                <MobileSecurityCard loading={loading} email={email} sessions={sessions} onOpenSessions={() => setPushed("sessions")} />
                <MobileNotificationsCard onOpen={() => setPushed("notifications")} />
                <SignOutButton />
              </>
            )}
          </Reveal>
        </StaggerItem>

        {/* ── Desktop (Figma cabinet/settings) ─────────────────────────────── */}
        <StaggerItem className="hidden gap-6 lg:flex">
          {/* Hand-written rail — uikit has no section-nav component, so the items carry their own focus ring. */}
          <nav aria-label={t("settings.a11y.sections")} className="flex w-53 shrink-0 flex-col gap-1">
            {NAV.map((item) => {
              const Icon = item.icon;
              const active = section === item.id;
              return (
                <button
                  key={item.id}
                  type="button"
                  aria-current={active ? "page" : undefined}
                  onClick={() => setSection(item.id)}
                  className={cn(
                    "flex items-center gap-3 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring",
                    active ? "bg-accent-debug/15 font-semibold text-accent-debug" : "text-ink hover:bg-ink/5",
                  )}
                >
                  <Icon className="size-4.5" />
                  {/* i18n-max: 20 — a 212px rail row less the icon and padding. */}
                  <span className="truncate">{t(item.labelKey)}</span>
                </button>
              );
            })}
          </nav>

          {/* Keyed on the section, so choosing one from the rail brings its pane in
              rather than swapping it under the cursor. The rail beside it does not
              remount, which is the point — the marker slides, the pane arrives. */}
          <Reveal key={section} className="min-w-0 flex-1">
            {section === "general" && <GeneralSection loading={loading} form={form} email={email} verified={!!profile?.email_verified} onChange={set} fieldErrors={fieldErrors} />}
            {section === "security" && <SecuritySection email={email} loading={loading} sessions={sessions} onManageSessions={() => setSection("sessions")} />}
            {section === "notifications" && <NotificationsSection />}
            {section === "sessions" && sessionsPanel(true)}
          </Reveal>
        </StaggerItem>
      </Stagger>
    </>
  );
}
