// Formatting + small helpers shared by the admin-console views. Money comes from the
// cabinet's one money module (`@/shared/lib/money`) — the console reads the same figures
// an investor does, so it shows them at the same precision.
//
// Nothing here holds a translator of its own: this is a plain module, so every helper that
// produces words takes the caller's `t`. The views are all client components, so `t` is
// one `useT()` away at every call site — and keeping it a parameter is what stops a second
// copy of the catalogue (or React) leaking into a module that only formats.

import type { Translate } from "@evinvest/i18n";

/** `formatAmount` is a decimal amount → grouped display with no currency symbol; the
 *  admin tables spell the unit out in the header instead. */
export { compactUnits, formatAmount as amount, formatNav, formatUnits, formatUsd, fractionOfCap, toBaseUnits } from "@/shared/lib/money";

/** A unix-seconds string → a coarse "3h ago" age (for queue/session rows).
 *
 *  The four buckets are plural messages rather than `${n}<unit> ago` concatenations: the
 *  English abbreviations do not inflect, but Russian and Vietnamese need the count to
 *  choose a form, and only ICU can hand them that choice. `#` also renders the number
 *  through the reader's locale, so a four-digit day count groups correctly. */
export function ago(unixSecs: string | undefined, t: Translate): string {
  const stamped = Number(unixSecs ?? "0");
  if (!stamped) return "—";
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - stamped);
  if (secs < 60) return t("admin.ago.seconds", "{n, plural, one {#s ago} other {#s ago}}", { n: secs });
  if (secs < 3600) return t("admin.ago.minutes", "{n, plural, one {#m ago} other {#m ago}}", { n: Math.floor(secs / 60) });
  if (secs < 86_400) return t("admin.ago.hours", "{n, plural, one {#h ago} other {#h ago}}", { n: Math.floor(secs / 3600) });
  return t("admin.ago.days", "{n, plural, one {#d ago} other {#d ago}}", { n: Math.floor(secs / 86_400) });
}

/**
 * The roles this console can actually grant, least→most privileged.
 *
 * `owner` is absent, and its absence is the rule rather than an omission. A seat is now a
 * persisted column, and `SetRole` refuses to grant or withdraw it unconditionally — the
 * only routes in are the consilium's admission and the genesis seed the service applies at
 * start-up, and the only routes out are a removal or a resignation. Offering the option
 * anyway is what produced the complaint this list answers: the console proposed a role
 * change and the plane answered `FAILED_PRECONDITION`, so the reader was refused an action
 * the interface had just held out to them.
 */
export const ASSIGNABLE_ROLES = ["investor", "operator", "admin"] as const;

/**
 * The whole role vocabulary (matches the domain `Role`).
 *
 * Derived from {@link ASSIGNABLE_ROLES} so the two cannot drift: this is what a role
 * *filter* offers and what {@link roleLabel} recognises — reading an owner is fine
 * everywhere, it is only writing one that the plane reserves for the consilium.
 */
export const ROLES = [...ASSIGNABLE_ROLES, "owner"] as const;

/**
 * The KYC tiers this console can set, least→most verified.
 *
 * The ladder is closed, and it is the plane's rather than this console's: it is written
 * down beside `kyc_level` in `contracts/proto/banking/v1/users.proto`, where the money
 * plane enforces it — 0 registered, 1 verified, 2 enhanced, 3 elevated, with both
 * directions of money movement gated on >= 1.
 *
 * Naming the four is what retires the free-typed number this control used to be. An empty
 * field, a `NaN` and a `-1` stop being values a reader can express, so they stop needing
 * to be rejected — the fix is the closed list, not a validator behind an open one.
 */
export const KYC_LEVELS = [0, 1, 2, 3] as const;

/** A tier from {@link KYC_LEVELS} — narrower than the `number` the wire carries, because
 *  what a reader may PICK is narrower than what the plane may hold (see
 *  {@link kycLevelLabel} on tiers this console does not know). */
export type KycLevel = (typeof KYC_LEVELS)[number];

// Wire vocabularies that also reach the screen as labels. The wire value is what goes
// back to the API and never changes; these maps only decide what a reader sees.
//
// One entry per wire value, each holding its own English — rather than a guard set and
// an interpolated key. Interpolation had two costs the table does not: the English lived
// only in the catalogue, so `evinvest-i18n-extract` could not see these at all, and an
// unguarded lookup would render `admin.state.reconciling` in a table cell. A value the
// hub adds tomorrow still falls back to the raw wire word, which is the shape the failure
// has today — a bare lowercase word.
const ROLE_LABELS: Record<string, (t: Translate) => string> = {
  investor: t => t("admin.role.investor", "Investor"),
  operator: t => t("admin.role.operator", "Operator"),
  admin: t => t("admin.role.admin", "Admin"),
  owner: t => t("admin.role.owner", "Owner"),
};

const STATUS_LABELS: Record<string, (t: Translate) => string> = {
  healthy: t => t("admin.status.healthy", "Healthy"),
  degraded: t => t("admin.status.degraded", "Degraded"),
  error: t => t("admin.status.error", "Error"),
  active: t => t("admin.status.active", "Active"),
  disabled: t => t("admin.status.disabled", "Disabled"),
  onboarding: t => t("admin.status.onboarding", "Onboarding"),
  staged: t => t("admin.status.staged", "Staged"),
  blocked: t => t("admin.status.blocked", "Blocked"),
  pending: t => t("admin.status.pending", "Pending"),
  review: t => t("admin.status.review", "Review"),
};

const KYC_LEVEL_LABELS: Record<number, (t: Translate) => string> = {
  0: t => t("admin.kyc.registered", "Registered"),
  1: t => t("admin.kyc.verified", "Verified"),
  2: t => t("admin.kyc.enhanced", "Enhanced"),
  3: t => t("admin.kyc.elevated", "Elevated"),
};

const STATE_LABELS: Record<string, (t: Translate) => string> = {
  draft: t => t("admin.state.draft", "Draft"),
  open: t => t("admin.state.open", "Open"),
  closed: t => t("admin.state.closed", "Closed"),
  queued: t => t("admin.state.queued", "Queued"),
  processing: t => t("admin.state.processing", "Processing"),
  completed: t => t("admin.state.completed", "Completed"),
  failed: t => t("admin.state.failed", "Failed"),
  cancelled: t => t("admin.state.cancelled", "Cancelled"),
};

/** A `Role` wire value as a reader sees it.
 *
 *  The catalogue values are authored in **display case**, and the render sites carry no
 *  `capitalize`. They used to do the opposite — lowercase values, cased by CSS — which is
 *  invisible while the value is a one-word English wire identifier and wrong the moment it
 *  is translated: `capitalize` title-cases every word, so "nhà đầu tư" rendered as "Nhà Đầu
 *  Tư" and "en cours" as "En Cours". Do not re-lowercase these on the assumption that CSS
 *  will fix them up.
 *
 *  An unrecognised value falls back to the raw wire word rather than `t()`, so a status the
 *  hub adds tomorrow shows as `reconciling` and not as `admin.status.reconciling`. */
export function roleLabel(role: string, t: Translate): string {
  return ROLE_LABELS[role]?.(t) ?? role;
}

/** A KYC tier as a reader sees it: the tier's NAME, not its ordinal.
 *
 *  The ladder is described in words where it is defined, so a control offering a bare "2"
 *  makes the reader carry the mapping in their head. A tier this console does not know —
 *  one the identity plane grows later — falls back to the `L{n}` short form the table cells
 *  already use. That fallback is translated, unlike {@link roleLabel}'s: a role has a wire
 *  word to fall back ON, and a level has only a number. */
export function kycLevelLabel(level: number, t: Translate): string {
  return KYC_LEVEL_LABELS[level]?.(t) ?? t("admin.users.kycLevelShort", "L{n}", { n: level });
}

/** A health/lifecycle status as a reader sees it (see {@link roleLabel} on casing and fallback). */
export function statusLabel(status: string, t: Translate): string {
  return STATUS_LABELS[status]?.(t) ?? status;
}

/** An allocation / withdrawal / payout state as a reader sees it. */
export function stateLabel(state: string, t: Translate): string {
  return STATE_LABELS[state]?.(t) ?? state;
}

// Chain rails, as the console names them. The network codes are proper nouns, but the
// words beside them ("Chain", "Open Network") are prose, so the whole label goes through
// the catalogue rather than being half-translated. Two screens render these — Revenue's
// rail chips and Treasury's per-rail cards — and they must agree.
const RAIL_LABELS: Record<string, (t: Translate) => string> = {
  bep20: t => t("admin.rail.bep20", "BEP20 · BNB Chain"),
  trc20: t => t("admin.rail.trc20", "TRC20 · TRON"),
  ton: t => t("admin.rail.ton", "TON · Open Network"),
  polygon: t => t("admin.rail.polygon", "Polygon · PoS"),
};

/** A rail's display name; a rail the hub adds later falls back to its bare wire code. */
export function railLabel(network: string, t: Translate): string {
  return RAIL_LABELS[network]?.(t) ?? network;
}

/** Tailwind token classes for a lifecycle/health status pill. */
export function statusTone(status: string): string {
  switch (status) {
    case "active":
    case "healthy":
      return "text-positive";
    case "onboarding":
    case "degraded":
    case "staged":
      return "text-accent-warn";
    case "blocked":
    case "disabled":
    case "error":
      return "text-accent-error";
    default:
      return "text-ink-soft";
  }
}
