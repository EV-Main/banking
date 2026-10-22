// The hub's role and health/lifecycle words, shared by the admin console and the profile:
// one name per wire value across the cabinet. Display case — see `roleLabel` in
// `views/admin/lib/format.ts` on why these are not lowercased for CSS to capitalise.

import type { Translate } from "@evinvest/i18n";

export const roleWords = (t: Translate): Readonly<Record<string, string>> => ({
  investor: t("admin.role.investor", "Investor"),
  operator: t("admin.role.operator", "Operator"),
  admin: t("admin.role.admin", "Admin"),
  owner: t("admin.role.owner", "Owner"),
});

export const statusWords = (t: Translate): Readonly<Record<string, string>> => ({
  healthy: t("admin.status.healthy", "Healthy"),
  degraded: t("admin.status.degraded", "Degraded"),
  error: t("admin.status.error", "Error"),
  active: t("admin.status.active", "Active"),
  disabled: t("admin.status.disabled", "Disabled"),
  onboarding: t("admin.status.onboarding", "Onboarding"),
  staged: t("admin.status.staged", "Staged"),
  blocked: t("admin.status.blocked", "Blocked"),
  // Statuses the UI expects to see even though the hub does not emit them yet. Without
  // them here the lookup falls through and the reader gets the bare wire
  // word — which is the safe failure, but a needless one for a value we can name.
  pending: t("admin.status.pending", "Pending"),
  review: t("admin.status.review", "Review"),
});
