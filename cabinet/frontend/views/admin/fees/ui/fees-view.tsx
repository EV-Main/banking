"use client";

// Admin console — what each fund charges, and collecting it.
//
// This screen is the reason the fee plane does anything at all. The sweeper has been
// running hourly since the plane landed, but a fund charges nothing until it has a policy
// row, and until now there was no surface that could write one. So every fund in
// production charges zero — not by choice, but because the switch was unreachable.
//
// Two things happen here and they are deliberately kept apart. The LEFT column prices a
// product: terms an investor reads before they subscribe, and changing them changes what
// people pay. The RIGHT column collects what those terms have already earned: units the
// sweeper clawed back, converted to cash in one bulk settlement per period. Pricing is a
// decision; collecting is bookkeeping.
//
// What this screen does NOT do is pay the money out. Once settled, fee cash lands in the
// `fee` claim, which is exactly what `Fund revenue` withdraws on-chain — same account, an
// existing pipeline with its own rail liquidity and dispatch gates. Duplicating a payout
// form here would give an operator two doors to the same money.

import { Landmark, Loader2, Percent } from "lucide-react";
import { useMemo, useState } from "react";

import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Input, Skeleton } from "@evinvest/uikit";

import { setFeePolicy, settleFeeShares } from "@/entities/admin/api/admin-client";
import { adminAllocationsResource, feeAssessmentsResource, feePoliciesResource, feeSharesResource } from "@/entities/admin/model/admin-resource";
import type { FeePolicy } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { TAG } from "@/shared/lib/cache-tags";
import { pct, toBps, toPercentInput } from "@/shared/lib/rate";
import { revalidateTag, useResource } from "@/shared/lib/resource";
import { ago, formatUnits, formatUsd } from "@/views/admin/lib/format";
import { StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

// `value` is the wire enum the money plane stores; `label` is only what a reader sees.
// Both lists are module scope, where no hook can run, so they carry a thunk and the
// `Choice` chips below resolve it against the reader's locale.
const BASES = [
  { value: "invested_capital", label: (t: Translate) => t("admin.fees.basis.investedCapital", "Invested capital") },
  { value: "market_value", label: (t: Translate) => t("admin.fees.basis.marketValue", "Market value") },
] as const;

const PERIODS = [
  { value: "monthly", label: (t: Translate) => t("admin.fees.period.monthly", "Monthly") },
  { value: "quarterly", label: (t: Translate) => t("admin.fees.period.quarterly", "Quarterly") },
  { value: "semi_annual", label: (t: Translate) => t("admin.fees.period.semiAnnual", "Every 6 months") },
  { value: "annual", label: (t: Translate) => t("admin.fees.period.annual", "Annually") },
] as const;

/** The house default, and what an unconfigured fund's form opens on: 2 and 20, no hurdle,
 *  charged on invested capital so the fee cannot swell with a mark the operator posted. */
const DEFAULTS = { management_bps: 200, performance_bps: 2000, hurdle_bps: 0, basis: "invested_capital", crystallization: "annual" };

export function FeesView() {
  const t = useT();
  const catalog = useResource(adminAllocationsResource);
  const policies = useResource(feePoliciesResource);
  const [service, setService] = useState("");

  const funds = catalog.data?.allocations ?? [];
  // The first fund is the default so the screen is useful without a click.
  const selected = service || funds[0]?.service || "";
  const byService = useMemo(() => new Map((policies.data?.policies ?? []).map((p) => [p.service, p])), [policies.data]);
  const policy = byService.get(selected) ?? null;

  const loading = catalog.isLoading || policies.isLoading;
  const error = catalog.data || !catalog.error ? null : errorMessage(catalog.error, t);

  return (
    <AdminScreen className="space-y-6">
      <AdminHeader eyebrow={t("nav.fees", "Fees")} title={t("admin.fees.title", "Fee terms and collection")} subtitle={t("admin.fees.subtitle", "What each fund charges its investors, and converting what it has already earned into cash.")} />

      {error && <ResourceError variant="alert" message={error} />}

      {loading ? (
        <StaggerItem>
          <Skeleton className="h-64 w-full" />
        </StaggerItem>
      ) : funds.length === 0 ? (
        <StaggerItem as={Empty} className="border">
          {/* `EmptyHeader` + `EmptyMedia variant="icon"` is the house form for a zero
              state — see the operations timeline. Without the media slot this was a grey
              sentence in a dashed box, which is exactly what the design rules call out. */}
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <Landmark />
            </EmptyMedia>
            <EmptyTitle>{t("admin.fees.noFunds", "No funds registered")}</EmptyTitle>
            <EmptyDescription>{t("admin.fees.noFundsHint", "A fee is a property of a product. Register an allocation first, then price it here.")}</EmptyDescription>
          </EmptyHeader>
        </StaggerItem>
      ) : (
        <>
          <FundPicker funds={funds.map((f) => ({ service: f.service, title: f.title }))} selected={selected} onSelect={setService} policies={byService} />
          <StaggerItem className="grid gap-5 lg:grid-cols-2">
            <PolicyCard key={selected} service={selected} policy={policy} />
            <CollectCard service={selected} />
          </StaggerItem>
          <StaggerItem>
            <AssessmentsCard service={selected} />
          </StaggerItem>
        </>
      )}
    </AdminScreen>
  );
}

/** One row per fund, with its rate inline so the operator can see at a glance which
 *  products are priced and which are still charging nothing. */
function FundPicker({
  funds,
  selected,
  onSelect,
  policies,
}: {
  funds: { service: string; title: string }[];
  selected: string;
  onSelect: (service: string) => void;
  policies: Map<string, FeePolicy>;
}) {
  const t = useT();
  return (
    <StaggerItem className="flex flex-wrap gap-2">
      {funds.map((fund) => {
        const policy = policies.get(fund.service);
        const active = fund.service === selected;
        return (
          <button
            key={fund.service}
            type="button"
            onClick={() => onSelect(fund.service)}
            aria-pressed={active}
            className={`rounded-lg border px-3 py-2 text-left text-sm transition-colors ${
              active ? "border-accent-debug bg-accent-debug/10" : "border-border hover:bg-muted/50"
            }`}
          >
            <span className="block font-medium">{fund.title}</span>
            <span className="block text-xs text-ink-soft">
              {policy?.configured ? `${pct(policy.management_bps)} / ${pct(policy.performance_bps)}` : t("admin.fees.noFee", "No fee")}
            </span>
          </button>
        );
      })}
    </StaggerItem>
  );
}

function PolicyCard({ service, policy }: { service: string; policy: FeePolicy | null }) {
  const t = useT();
  const initial = policy?.configured ? policy : { ...DEFAULTS };
  // The stored policy is basis points; the form is percent. Seeding through
  // `toPercentInput` is what keeps that round trip lossless — a 2.55% rate opens on
  // "2.55" and saves back as the same 255 bps if the operator never touches it.
  const [management, setManagement] = useState(() => toPercentInput(initial.management_bps));
  const [performance, setPerformance] = useState(() => toPercentInput(initial.performance_bps));
  const [hurdle, setHurdle] = useState(() => toPercentInput(initial.hurdle_bps));
  const [basis, setBasis] = useState(initial.basis);
  const [crystallization, setCrystallization] = useState(initial.crystallization);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  // One conversion, read by the validator, the showcase and the save alike, so the figure
  // the operator is shown before saving is the same integer the request carries. `null`
  // means "does not parse as a percent" — the only state a field can hold that has no bps.
  const bps = useMemo(
    () => ({ management: toBps(management), performance: toBps(performance), hurdle: toBps(hurdle) }),
    [management, performance, hurdle],
  );

  // 100% is the domain's cap on every rate (`FeePolicy::new`): a fee larger than the thing
  // it is charged on is a fat finger, never a term. The old hundredfold-overcharge risk —
  // a percentage typed into a basis-points field — is gone by construction now that the
  // field *is* percent: 200 typed here means 200%, which this refuses outright.
  const invalid = useMemo(() => {
    for (const [name, value] of [
      [t("admin.fees.field.management", "Management"), bps.management],
      [t("admin.fees.field.performance", "Performance"), bps.performance],
      [t("admin.fees.field.hurdle", "Hurdle"), bps.hurdle],
    ] as const) {
      // The field name is interpolated rather than concatenated onto the front: which end
      // of the sentence it belongs at is a per-language decision.
      if (value === null) return t("admin.fees.err.notPercent", "{field} must be a percentage — 2, or 2.5 for a half.", { field: name });
      if (value > 10_000) return t("admin.fees.err.overHundred", "{field} cannot be more than 100%.", { field: name });
    }
    return null;
  }, [bps, t]);

  async function save() {
    // The null checks restate what `invalid` has already proved; they are here so the
    // types agree that every rate reaching the wire is a number.
    if (invalid || bps.management === null || bps.performance === null || bps.hurdle === null) return;
    setBusy(true);
    setProblem(null);
    setSaved(false);
    try {
      await setFeePolicy({
        service,
        management_bps: bps.management,
        performance_bps: bps.performance,
        hurdle_bps: bps.hurdle,
        basis,
        crystallization,
      });
      revalidateTag(TAG.adminFees);
      setSaved(true);
    } catch (e) {
      setProblem(e instanceof Error ? errorMessage(e, t) : t("err.feePolicySave", "Could not save these terms."));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card className="h-fit">
      <CardContent className="space-y-4 py-6">
        <div className="space-y-1">
          <p className="text-sm font-semibold">{t("admin.fees.terms", "Terms")}</p>
          <p className="text-xs text-ink-soft">
            {policy?.configured ? t("admin.fees.lastChanged", "Last changed {when}. Investors see these on the product page.", { when: ago(policy.updated_at, t) }) : t("admin.fees.notConfigured", "This fund charges nothing today. Saving these terms starts the clock on every holding in it.")}
          </p>
        </div>

        <div className="grid gap-3 sm:grid-cols-3">
          <PercentField label={t("admin.fees.field.management", "Management")} value={management} onChange={setManagement} hint={t("admin.fees.hint.perYear", "per year")} />
          <PercentField label={t("admin.fees.field.performance", "Performance")} value={performance} onChange={setPerformance} hint={t("admin.fees.hint.ofTheGain", "of the gain")} />
          <PercentField label={t("admin.fees.field.hurdle", "Hurdle")} value={hurdle} onChange={setHurdle} hint={t("admin.fees.hint.zeroForNone", "0 for none")} />
        </div>

        <Showcase bps={bps} />

        <Choice label={t("admin.fees.chargedOn", "Charged on")} value={basis} onChange={setBasis} options={BASES} />
        <p className="text-xs text-ink-soft">{t("admin.fees.basisNote", "Invested capital is the house default: it does not swell with a mark you posted, so what the manager earns stays independent of the input the manager supplies.")}</p>

        <Choice label={t("admin.fees.lockedIn", "Performance locked in")} value={crystallization} onChange={setCrystallization} options={PERIODS} />
        <p className="text-xs text-ink-soft">{t("admin.fees.crystallizationNote", "A price, not a detail — crystallizing more often measurably raises what an investor pays over a fund's life, because each reset locks in gains a later loss can no longer claw back.")}</p>

        {invalid && <p className="text-xs text-accent-error">{invalid}</p>}
        {problem && <p className="text-xs text-accent-error">{problem}</p>}
        {saved && !problem && <p className="text-xs text-positive">{t("admin.fees.saved", "Saved. Investors see the new terms immediately.")}</p>}

        <Button type="button" onClick={save} disabled={busy || invalid !== null}>
          {busy && <Loader2 className="size-4 animate-spin" />}
          {policy?.configured ? t("admin.fees.updateTerms", "Update terms") : t("admin.fees.startCharging", "Start charging")}
        </Button>
      </CardContent>
    </Card>
  );
}

/** Accumulated units and the one button that turns them into cash. */
function CollectCard({ service }: { service: string }) {
  const t = useT();
  const shares = useResource(feeSharesResource, service);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);

  const data = shares.data ?? null;
  const nothing = !data || Number(data.units) <= 0;

  async function settle() {
    setBusy(true);
    setProblem(null);
    setDone(null);
    try {
      // Empty units settles the whole balance — the ordinary end-of-period call, and the
      // only one worth a button. A partial settlement is a rare, deliberate act better
      // done against the API than offered as a field nobody needs.
      const settlement = await settleFeeShares({ service, units: "" });
      // The settle moves the fee units AND the revenue figure the payout screen reads.
      revalidateTag(TAG.adminFees, TAG.adminRevenue);
      setDone(t("admin.fees.settledAtNav", "{cash} settled at NAV {nav}.", { cash: formatUsd(settlement.cash), nav: settlement.nav }));
    } catch (e) {
      setProblem(e instanceof Error ? errorMessage(e, t) : t("err.feeSettle", "Could not settle these units."));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card className="h-fit">
      <CardContent className="space-y-4 py-6">
        <div className="space-y-1">
          <p className="text-sm font-semibold">{t("admin.fees.collected", "Collected, not yet converted")}</p>
          <p className="text-xs text-ink-soft">{t("admin.fees.collectedSub", "Units the sweeper has clawed back from holders. No cash has moved yet.")}</p>
        </div>

        {shares.isLoading ? (
          <Skeleton className="h-16 w-full" />
        ) : (
          <dl className="space-y-2.5 text-sm">
            <Row label={t("admin.fees.unitsHeld", "Fee units held")} value={formatUnits(data?.units)} />
            <Row label={t("admin.fees.worthAtNav", "Worth at current NAV")} value={`${formatUsd(data?.value)} USDT`} />
          </dl>
        )}

        <p className="text-xs text-ink-soft">{t("admin.fees.settleNote", "Settling burns these units and moves their value from the fund's claim into fee revenue. It is refused — never queued — when the fund's claim cannot cover it on top of its queued redemptions: investors waiting to exit are paid before the manager is.")}</p>

        {problem && <p className="text-xs text-accent-error">{problem}</p>}
        {/* Two independently complete sentences, so the settlement line and the pointer to
            the payout screen stay separate keys; the screen's own name is interpolated so it
            tracks whatever the nav calls it. The emphasis on that name is the one casualty
            of keeping the sentence whole for translators. */}
        {done && !problem && <p className="text-xs text-positive">{`${done} ${t("admin.fees.withdrawableFrom", "It is withdrawable on-chain from {screen}.", { screen: t("nav.revenue", "Fund revenue") })}`}</p>}

        <Button type="button" variant="outline" onClick={settle} disabled={busy || nothing}>
          {busy && <Loader2 className="size-4 animate-spin" />}
          {t("admin.fees.settleAll", "Settle all units")}
        </Button>
        {nothing && !shares.isLoading && <p className="text-xs text-ink-soft">{t("admin.fees.nothingToSettle", "Nothing to settle — no fee has been charged in this fund yet.")}</p>}
      </CardContent>
    </Card>
  );
}

/** The audit trail: every charge this fund has made, newest first. */
function AssessmentsCard({ service }: { service: string }) {
  const t = useT();
  const list = useResource(feeAssessmentsResource, service);
  const rows = list.data?.assessments ?? [];

  return (
    <Card>
      <CardContent className="space-y-4 py-6">
        <p className="text-sm font-semibold">{t("admin.fees.charges", "Charges")}</p>
        {list.isLoading ? (
          <Skeleton className="h-24 w-full" />
        ) : rows.length === 0 ? (
          <Empty className="border">
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <Percent />
              </EmptyMedia>
              <EmptyTitle>{t("admin.fees.noCharges", "No charges yet")}</EmptyTitle>
              <EmptyDescription>{t("admin.fees.noChargesHint", "The sweeper charges a holding once its accrual is old enough. Nothing here means nobody has been billed.")}</EmptyDescription>
            </EmptyHeader>
          </Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                {/* i18n-max: 14 per header — the wrapper scrolls, so a long header costs a
                    sideways drag rather than a clipped column. */}
                <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-ink-soft">
                  <th className="pb-2 font-medium">{t("admin.col.when", "When")}</th>
                  <th className="pb-2 font-medium">{t("admin.fees.col.trigger", "Trigger")}</th>
                  <th className="pb-2 text-right font-medium">{t("admin.fees.field.management", "Management")}</th>
                  <th className="pb-2 text-right font-medium">{t("admin.fees.field.performance", "Performance")}</th>
                  <th className="pb-2 text-right font-medium">{t("admin.fees.col.unitsTaken", "Units taken")}</th>
                  <th className="pb-2 text-right font-medium">{t("admin.fees.col.deferred", "Deferred")}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((a, i) => (
                  <tr key={`${a.assessed_at}-${i}`} className="border-b border-border/50 last:border-0">
                    <td className="py-2 text-ink-soft">{ago(a.assessed_at, t)}</td>
                    <td className="py-2 capitalize">{a.trigger}</td>
                    <td className="py-2 text-right tabular-nums">{formatUsd(a.management)}</td>
                    <td className="py-2 text-right tabular-nums">{formatUsd(a.performance)}</td>
                    <td className="py-2 text-right tabular-nums">{formatUnits(a.charged_units)}</td>
                    {/* Non-zero means the holding could not cover the charge and the rest
                        rides to the next one. Worth its own column: it is the only reason
                        a charge collects less than it assessed. */}
                    <td className={`py-2 text-right tabular-nums ${Number(a.debt_carried) > 0 ? "text-accent-warn" : "text-ink-soft"}`}>
                      {formatUsd(a.debt_carried)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/** A rate, as a term sheet states one. The percent sign is furniture inside the field
 *  rather than a character the operator types, so what the value means is legible while
 *  the box still holds nothing but the number `toBps` parses. */
function PercentField({ label, value, onChange, hint }: { label: string; value: string; onChange: (v: string) => void; hint: string }) {
  return (
    <label className="space-y-1.5 text-sm">
      <span className="block font-medium">{label}</span>
      <div className="relative">
        {/* `decimal` rather than `numeric`: half a percent is a rate someone will charge,
            and a numeric keypad on a phone has no decimal separator. */}
        <Input inputMode="decimal" value={value} onChange={(e) => onChange(e.target.value)} className="pr-8 tabular-nums" />
        {/* Inside the `label`, so it joins the field's accessible name — "Management %
            per year". Not decorative: the unit is the whole point of this screen's
            change, and a reader who cannot see it is the one who most needs telling. */}
        <span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-sm text-ink-soft">%</span>
      </div>
      <span className="block text-xs text-ink-soft">{hint}</span>
    </label>
  );
}

/** The reference position the worked example prices. A round hundred thousand: big enough
 *  that the management line lands on a figure worth arguing about, round enough that a
 *  reader can move the decimal point to their own fund size in their head. */
const REFERENCE_POSITION = 100_000;

/** What the percentages above come to.
 *
 *  The form takes a price the way a term sheet states one — "2 and 20" — while the money
 *  plane stores and charges basis points. Both belong on screen: the bps figure is what
 *  this screen is about to write, and the money figure is the only form in which a fee is
 *  actually argued about. Neither is an input, so nothing here can drift from what saves. */
function Showcase({ bps }: { bps: { management: number | null; performance: number | null; hurdle: number | null } }) {
  const t = useT();

  const rows = [
    { key: "management", label: t("admin.fees.field.management", "Management"), value: bps.management },
    { key: "performance", label: t("admin.fees.field.performance", "Performance"), value: bps.performance },
    { key: "hurdle", label: t("admin.fees.field.hurdle", "Hurdle"), value: bps.hurdle },
  ] as const;

  // Withheld rather than guessed while any rate is unparsable: a sentence assembled from
  // a field the form is about to reject would price terms nobody can save.
  const example =
    bps.management === null || bps.performance === null || bps.hurdle === null
      ? null
      : (bps.hurdle > 0 ? t("admin.fees.showcase.exampleHurdle", "On a {amount} position: {management} a year, plus {performance} of any gain above a mark rising {hurdle} a year.", {
          amount: formatUsd(REFERENCE_POSITION),
          management: formatUsd((REFERENCE_POSITION * bps.management) / 10_000),
          performance: pct(bps.performance),
          hurdle: pct(bps.hurdle),
        }) : t("admin.fees.showcase.example", "On a {amount} position: {management} a year, plus {performance} of any gain.", {
          amount: formatUsd(REFERENCE_POSITION),
          management: formatUsd((REFERENCE_POSITION * bps.management) / 10_000),
          performance: pct(bps.performance),
          hurdle: pct(bps.hurdle),
        }));

  return (
    <div className="space-y-3 rounded-lg border border-border bg-muted/30 p-3">
      <p className="text-xs font-medium text-ink-soft">{t("admin.fees.showcase.title", "What gets saved")}</p>
      <dl className="grid gap-3 sm:grid-cols-3">
        {rows.map((row) => (
          <div key={row.key} className="space-y-0.5">
            <dt className="text-xs text-ink-soft">{row.label}</dt>
            <dd className="text-sm tabular-nums">
              {row.value === null ? (
                <span className="text-ink-soft">—</span>
              ) : (
                <>
                  <span className="font-medium">{pct(row.value)}</span>{" "}
                  <span className="text-xs text-ink-soft">{t("admin.fees.showcase.bps", "{n} bps", { n: row.value })}</span>
                </>
              )}
            </dd>
          </div>
        ))}
      </dl>
      {example && <p className="text-xs text-ink-soft">{example}</p>}
    </div>
  );
}

function Choice<T extends string>({
  label,
  value,
  onChange,
  options,
}: {
  label: string;
  value: string;
  onChange: (v: T) => void;
  options: readonly { value: T; label: (t: Translate) => string }[];
}) {
  const t = useT();
  return (
    <div className="space-y-1.5 text-sm">
      <span className="block font-medium">{label}</span>
      <div className="flex flex-wrap gap-2">
        {options.map((option) => (
          <button
            key={option.value}
            type="button"
            onClick={() => onChange(option.value)}
            aria-pressed={value === option.value}
            className={`rounded-md border px-2.5 py-1.5 text-xs transition-colors ${
              value === option.value ? "border-accent-debug bg-accent-debug/10" : "border-border hover:bg-muted/50"
            }`}
          >
            {option.label(t)}
          </button>

        ))}
      </div>
    </div>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt className="text-ink-soft">{label}</dt>
      <dd className="font-medium tabular-nums">{value}</dd>
    </div>
  );
}
