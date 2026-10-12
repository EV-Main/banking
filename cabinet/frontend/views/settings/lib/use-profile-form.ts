"use client";

// The settings form behind both panes: the profile read, the seeded editable copy, the
// save and its outcome. One hook, because Preferences and Personal details edit the same
// core user record (full-replace, so the one form carries every editable field whichever
// pane is open) and the mobile row cards and the desktop sections must not each keep a
// copy of "is this dirty".

import { isLocale } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";
import { useEffect, useRef, useState } from "react";

import { profileResource, saveProfile } from "@/entities/user/model/profile-resource";
import { validateProfileForm } from "@/entities/user/model/profile-schema";
import { relocalise } from "@/shared/config/base-path";
import type { UpdateProfileRequest, UserProfile } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { confirmLocaleCookie } from "@/shared/lib/locale-cookie";
import { type ResourceSeed, useSeededResource } from "@/shared/lib/resource";
import { EDITABLE, type Form, formFrom } from "@/views/settings/lib/form";

/** `initial` is the server's read of the profile, when the route made one (see `SettingsView`). */
export function useProfileForm(initial?: ResourceSeed<UserProfile>) {
  const t = useT();
  const locale = useLocale();
  const [form, setForm] = useState<Form | null>(null);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const savedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});

  // The same cached read the account chip and the Profile page use.
  const { data: profile, error: profileError, isLoading: loading } = useSeededResource(profileResource, initial);
  const error = saveError ?? (profile || !profileError ? null : errorMessage(profileError, t));

  // Seeded during render, not in an effect, so a cached profile fills the form on the frame
  // it is read. Held back while the form is dirty: a background refresh must not overwrite
  // edits in progress.
  const [seeded, setSeeded] = useState<UserProfile | null>(null);
  const pristine = !form || !seeded || EDITABLE.every((k) => form[k] === (seeded[k] ?? ""));
  if (profile && profile !== seeded && pristine) {
    setSeeded(profile);
    setForm(formFrom(profile));
  }

  function set(key: keyof Form, value: string) {
    setSaved(false);
    setForm((f) => (f ? { ...f, [key]: value } : f));
  }
  async function save() {
    if (!form || saving) return;
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
      // Confirmed even when the page already speaks it: this is the one moment the
      // reader has said which language this device keeps, and a cookie seeded from a
      // URL must not outrank it later.
      if (isLocale(chosen)) confirmLocaleCookie(chosen);
      if (isLocale(chosen) && chosen !== locale) {
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

  return { profile, loading, error, form, dirty: !pristine, saving, saved, fieldErrors, set, save };
}
