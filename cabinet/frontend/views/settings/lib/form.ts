// The shape of the settings form: the profile fields this surface edits, and the option
// lists the three choice fields pick from. Pure, so the mobile row editors and the desktop
// General form work from one field set and one set of labels.

import { LOCALES, LOCALE_LABELS, type Translate } from "@evinvest/i18n";

import type { UserProfile } from "@/shared/contracts";

export const EDITABLE = ["legal_name", "preferred_name", "phone", "date_of_birth", "nationality", "tax_residence", "residential_address", "language", "base_currency", "timezone"] as const;
export type Form = Record<(typeof EDITABLE)[number], string>;

export function formFrom(p: UserProfile): Form {
  return Object.fromEntries(EDITABLE.map((k) => [k, p[k] ?? ""])) as Form;
}

/** A choice ready to render: the stored wire value and the words shown for it. */
export type Option = { value: string; label: string };
/**
 * A choice as this module declares it. `label` carries its own English, so the
 * `optionsOf` resolves at the call site — this file is a plain module with no
 * translator, and the finished English it used to hold left the currency and
 * time-zone menus in English under every other locale. `label` is for the handful
 * of values that are the same word everywhere.
 */
type CatalogOption = { value: string; label?: (t: Translate) => string };

// Every locale the platform publishes, each labelled in its own language — a reader who
// cannot read the current language cannot read "Russian" either, which is why
// @evinvest/i18n owns the labels rather than a catalogue key. Derived from LOCALES so
// launching a sixth locale needs no edit here; the two hardcoded options this replaced
// meant the other three were unreachable from the cabinet even though every route,
// catalogue and redirect already supported them.
export const LANGUAGES: Option[] = LOCALES.map((value) => ({ value, label: LOCALE_LABELS[value] }));
export const CURRENCIES: CatalogOption[] = [
  { value: "USD", label: t => t("settings.currency.usd", "USD ($)") },
  { value: "EUR", label: t => t("settings.currency.eur", "EUR (€)") },
];
export const TIMEZONES: CatalogOption[] = [
  { value: "Asia/Ho_Chi_Minh", label: t => t("settings.tz.hoChiMinh", "Asia / Ho Chi Minh") },
  // An abbreviation that reads the same in every locale we publish, so it carries no key.
  { value: "UTC" },
];

/** Resolve declared choices into renderable ones, in the reader's locale. */
export function optionsOf(options: readonly CatalogOption[], t: Translate): Option[] {
  return options.map((o) => ({ value: o.value, label: o.label?.(t) ?? o.value }));
}

export function labelOf(options: Option[], value: string): string {
  return options.find((o) => o.value === value)?.label ?? value;
}
