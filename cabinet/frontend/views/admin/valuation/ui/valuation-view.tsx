"use client";

import { Loader2, TriangleAlert } from "lucide-react";
import { useState } from "react";

import type { Translate } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Input, Select, SelectContent, SelectItem, SelectTrigger, Skeleton } from "@evinvest/uikit";

import { failRedemption, postValuation, setAllocationUnitCap, settleRedemption } from "@/entities/admin/api/admin-client";
import { adminAllocationsResource, redemptionQueueResource } from "@/entities/admin/model/admin-resource";
import { fundNavResource } from "@/entities/fund/model/fund-resource";
import type { Allocation } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { TAG } from "@/shared/lib/cache-tags";
import { cn } from "@/shared/lib/cn";
import { revalidateTag, useResource } from "@/shared/lib/resource";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { TipAnchor } from "@/shared/tips";
import { ago, compactUnits, formatNav, formatUnits, formatUsd, fractionOfCap, stateLabel, toBaseUnits } from "@/views/admin/lib/format";
import { AdminHeader, AdminScreen, Toggle } from "@/views/admin/ui/shell";

const TEAL_CTA = "bg-accent-debug text-background hover:bg-accent-debug/90";

// "EV Trading (trading)", with the state trailing when it is not the plain open case.
// Two keys rather than one with an optional tail: the punctuation joining a name to a
// state is a per-language choice, and an empty `{state}` would leave a dangling dash.
function allocationLabel(a: Allocation, t: Translate): string {
  const values = { title: a.title, service: a.service };
  return a.state === "open"
    ? t("admin.valuation.allocationLabel", values)
    : t("admin.valuation.allocationLabelWithState", { ...values, state: stateLabel(a.state, t) });
}

export function ValuationView() {
  const t = useT();
  const locale = useLocale();
  // The fund is PICKED from the registry, never typed: the hub refuses a valuation for
  // an unregistered service, so a free-text field could only ever produce a NOT_FOUND.
  // Drafts and closed products are listed too — a closed fund still gets marked so its
  // queued redemptions price correctly.
  const [service, setService] = useState("");
  const [aum, setAum] = useState("");
  const [override, setOverride] = useState(false);
  const [posting, setPosting] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const registry = useResource(adminAllocationsResource);
  const queueRead = useResource(redemptionQueueResource);
  // The same per-fund NAV entry the investor screens read, so a mark posted here lands on
  // the product page and the fund cards without either of them re-fetching.
  const navRead = useResource(fundNavResource, service);

  const allocations = registry.data ? (registry.data.allocations ?? []) : null;
  const queue = queueRead.data ? (queueRead.data.items ?? []) : null;
  const nav = navRead.data ?? null;
  const error =
    actionError ??
    (allocations || !registry.error ? null : errorMessage(registry.error, t)) ??
    (queue || !queueRead.error ? null : errorMessage(queueRead.error, t));

  // Default to the first product that can actually take a mark, so the common case needs no
  // interaction; fall back to the first row when none is open yet. Chosen during render, so
  // a cached registry means the form arrives already pointed at a fund.
  const [picked, setPicked] = useState(false);
  if (!picked && allocations) {
    setPicked(true);
    const initial = allocations.find((a) => a.state === "open") ?? allocations[0];
    if (initial) setService(initial.service);
  }

  // Live derived NAV preview = entered AUM / current units. NAV is *derived*, never
  // entered — hence the read-only box below.
  const units = Number(nav?.units_outstanding ?? "0");
  const aumNum = Number(aum || "0");
  const derivedNav = units > 0 && aumNum > 0 ? aumNum / units : null;
  const currentNav = derivedNav ?? Number(nav?.nav ?? "0");
  // A fund nobody has subscribed to has no units, so AUM / units is undefined and the
  // hub rejects the post outright (`nav undefined: no units outstanding`). Say so here
  // instead of letting the operator fill the form and meet a raw domain error — but gate
  // only the POST. Writing the figure down is not what is impossible, so the AUM field
  // stays usable.
  const noUnits = nav !== null && units === 0;
  const selected = allocations?.find((a) => a.service === service) ?? null;

  const post = async () => {
    setPosting(true);
    setActionError(null);
    try {
      // The POST answers with the new mark, so it is published straight in rather than
      // re-read — and every investor surface showing this fund's price follows.
      fundNavResource.publish(await postValuation({ service, aum, override }), service);
      setAum("");
      await queueRead.refresh();
    } catch (e) {
      setActionError(errorMessage(e, t));
    } finally {
      setPosting(false);
    }
  };

  const act = async (fn: (id: string) => Promise<unknown>, id: string) => {
    setBusy(id);
    setActionError(null);
    try {
      await fn(id);
      // Settling burns units, so the derived-NAV preview and the queue's est-cash go stale
      // on the units_outstanding this screen loaded with — and so do the investor's own
      // position and redemption lists. One tag sweep covers all of them.
      revalidateTag(TAG.nav, TAG.positions, TAG.redemptions, TAG.operations);
      await Promise.all([queueRead.refresh(), navRead.refresh()]);
    } catch (e) {
      setActionError(errorMessage(e, t));
    } finally {
      setBusy(null);
    }
  };

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader eyebrow={t("admin.eyebrow.administer")} title={t("nav.valuation")} subtitle={t("admin.valuation.subtitle")} />

      {error && <ResourceError message={error} />}

      <StaggerItem as="section" className="space-y-3" id="post-valuation">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.valuation.postValuation")}</p>
        <Card>
          <CardContent className="space-y-5 py-6">
            <div className="grid gap-4 md:grid-cols-3">
              <div className="flex flex-col gap-1.5">
                <span className="text-sm text-ink-soft">{t("admin.valuation.fundService")}</span>
                <Select
                  value={service || undefined}
                  onValueChange={setService}
                >
                  {/* `disabled` lives on the trigger — `Select` itself is a pure state
                      container and takes no such prop. */}
                  <SelectTrigger className="w-full border-border bg-secondary" disabled={!allocations || allocations.length === 0}>
                    {/* Not `SelectValue`: the uikit's renders the raw stored value, so the
                        trigger would read the bare slug instead of the product's title. */}
                    <span className={cn("truncate", !selected && "text-ink-soft")}>
                      {selected ? allocationLabel(selected, t) : !allocations ? t("ui.loading") : t("admin.valuation.noAllocations")}
                    </span>
                  </SelectTrigger>
                  <SelectContent>
                    {(allocations ?? []).map((a) => (
                      <SelectItem key={a.service} value={a.service}>
                        {allocationLabel(a, t)}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <label className="flex flex-col gap-1.5">
                <span className="flex items-center gap-1.5 text-sm text-ink-soft">
                  {t("admin.valuation.aumUsdt")}
                  <TipAnchor anchor="admin.valuation.post.aum" />
                </span>
                <Input value={aum} onChange={(e) => setAum(e.target.value)} inputMode="decimal" placeholder="0.00" className="w-full" />
              </label>
              <div className="flex flex-col gap-1.5">
                <span className="flex items-center gap-1.5 text-sm text-ink-soft">
                  {t("admin.valuation.derivedNav")}
                  <TipAnchor anchor="admin.valuation.post.derived-nav" />
                </span>
                {/* Read-only on purpose: NAV is derived (AUM / units read live from the
                    ledger), never posted directly — an editable field here would imply
                    an operator can set a price. */}
                <div className="flex h-9 items-center rounded-md border border-accent-debug/40 bg-accent-debug/10 px-3 text-sm" aria-readonly="true">
                  <span className="font-semibold text-accent-debug tabular-nums">{derivedNav ? formatNav(derivedNav) : "—"}</span>
                  {/* An ICU plural, so `units` agrees with the count and `#` groups the
                      digits in the reader's convention — the hard-coded `en-US` is gone. */}
                  {units > 0 && <span className="ml-2 text-xs tabular-nums text-ink-soft">{t("admin.valuation.derivedFormula", { n: units })}</span>}
                </div>
              </div>
            </div>

            {noUnits ? (
              <div className="rounded-lg border border-border bg-ink/5 px-4 py-2.5 text-sm text-ink-soft">
                <TriangleAlert className="mr-2 inline size-4" />
                {/* The emphasised fragment is the formula, mid-sentence — an ICU argument
                    rather than a cut, so a translator gets the whole thought. */}
                {t("admin.valuation.noUnitsNote", { formula: t("admin.valuation.navFormula") })}
              </div>
            ) : (
              <div className="rounded-lg border border-accent-warn/30 bg-accent-warn/5 px-4 py-2.5 text-sm text-accent-warn">
                <TriangleAlert className="mr-2 inline size-4" />
                {t("admin.valuation.navGuardNote")}
              </div>
            )}

            <div className="flex items-center gap-4">
              <div className="flex items-center gap-2">
                <Toggle on={override} onChange={setOverride} label={t("admin.valuation.overrideGuard")} />
                <div className="text-sm">
                  <p className="flex items-center gap-1.5">
                    {t("admin.valuation.overrideGuard")}
                    <TipAnchor anchor="admin.valuation.post.override" />
                  </p>
                  <p className="text-xs text-ink-soft">{t("admin.valuation.overrideGuardHint")}</p>
                </div>
              </div>
              <Button type="button" className={cn("ml-auto", TEAL_CTA)} disabled={posting || !aum || !service || noUnits} onClick={post}>
                {posting ? <Loader2 className="size-4 animate-spin" /> : null}
                {t("admin.valuation.postValuation")}
              </Button>
            </div>
          </CardContent>
        </Card>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="flex items-center gap-2 text-xs font-semibold uppercase tracking-widest text-ink-soft">
          {t("admin.valuation.unitSupply")}
          <TipAnchor anchor="admin.valuation.post.derived-nav" />
        </p>
        <SupplyCapCard
          allocation={selected}
          nav={nav}
          onSaved={(updated) => {
            // The cap answer carries the whole allocation, so it is written straight into
            // the registry entry — and the cap gates money, so the investor-facing catalog
            // and this fund's mark are told too.
            const rows = registry.data?.allocations ?? [];
            adminAllocationsResource.publish({ ...registry.data, allocations: rows.map((a) => (a.service === updated.service ? updated : a)) });
            revalidateTag(TAG.catalog);
            void navRead.refresh();
          }}
          onError={setActionError}
        />
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="flex items-center gap-2 text-xs font-semibold uppercase tracking-widest text-ink-soft">
          {t("admin.valuation.redemptionQueue")}
          {/* The count pill lands on the same step as the label it trails, so its fill and
              accent colour — not a smaller size — are what set it apart. */}
          {queue && (
            <span className="whitespace-nowrap rounded-full bg-accent-warn/15 px-2 py-0.5 text-xs font-semibold text-accent-warn">
              {t("admin.valuation.queuedCount", { n: queue.length })}
            </span>
          )}
        </p>
        <Card>
          <CardContent className="p-0">
            <Settled
              loading={!queue}
              skeleton={
                <div className="p-6">
                  <Skeleton className="h-32 w-full" />
                </div>
              }
            >
              {!queue ? null : queue.length === 0 ? (
                <p className="p-8 text-center text-sm text-ink-soft">{t("admin.valuation.queueEmpty")}</p>
              ) : (
                <table className="w-full text-sm">
                  <thead>
                    {/* i18n-max: 14 per header — auto-layout table with no scroll wrapper. */}
                    <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-ink-soft">
                      <th className="px-5 py-3 font-medium">{t("admin.col.user")}</th>
                      <th className="px-5 py-3 font-medium">{t("invest.units")}</th>
                      <th className="px-5 py-3 font-medium">
                        <span className="flex items-center gap-1.5">
                          {t("admin.valuation.col.estCash")}
                          <TipAnchor anchor="admin.valuation.queue.est-cash" />
                        </span>
                      </th>
                      <th className="px-5 py-3 font-medium">{t("admin.col.age")}</th>
                      <th className="px-5 py-3 text-right font-medium">{t("admin.col.actions")}</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-border">
                    {queue.map((item) => {
                      const est = currentNav > 0 ? Number(item.units) * currentNav : null;
                      return (
                        <tr key={item.redemption_id}>
                          <td className="px-5 py-3">
                            <p className="font-medium">{item.email || item.user_id.slice(0, 8)}</p>
                            <p className="font-mono-tech text-xs text-ink-soft">{item.service}</p>
                          </td>
                          {/* A bare unit count, grouped in the reader's locale rather than
                              `en-US` — it is not money, so `shared/lib/money.ts` has no
                              say here. */}
                          <td className="px-5 py-3 tabular-nums">{Number(item.units).toLocaleString(locale)}</td>
                          <td className="px-5 py-3 tabular-nums text-ink-soft">{est ? t("admin.valuation.approx", { amount: formatUsd(est) }) : "—"}</td>
                          <td className="px-5 py-3 text-ink-soft">{ago(item.created_at, t)}</td>
                          <td className="px-5 py-3">
                            {/* i18n-max: 12 per verb — two `shrink-0` Buttons, each with a
                                tip anchor, share this cell. */}
                            <div className="flex justify-end gap-2">
                              <span className="inline-flex items-center gap-1">
                                <Button type="button" variant="outline" size="sm" disabled={busy === item.redemption_id} onClick={() => act(settleRedemption, item.redemption_id)}>
                                  {t("admin.settle")}
                                </Button>
                                <TipAnchor anchor="admin.valuation.queue.settle" />
                              </span>
                              <span className="inline-flex items-center gap-1">
                                <Button
                                  type="button"
                                  variant="outline"
                                  size="sm"
                                  className="border-accent-error/40 text-accent-error hover:bg-accent-error/10"
                                  disabled={busy === item.redemption_id}
                                  onClick={() => act(failRedemption, item.redemption_id)}
                                >
                                  {t("admin.fail")}
                                </Button>
                                <TipAnchor anchor="admin.valuation.queue.fail" />
                              </span>
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              )}
            </Settled>
          </CardContent>
        </Card>
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.valuation.queueFootnote")}</p>
      </StaggerItem>
    </AdminScreen>
  );
}

/// How many units this product may ever issue. Lives on the allocation, not on a
/// valuation mark: a mark is an immutable historical price, while the cap is a policy an
/// operator revises — storing it on the mark would make "change the cap" mean "post a
/// price", and tangle the two histories together.
function SupplyCapCard({
  allocation,
  nav,
  onSaved,
  onError,
}: {
  allocation: Allocation | null;
  nav: { units_outstanding?: string; remaining_capacity?: string } | null;
  onSaved: (updated: Allocation) => void;
  onError: (message: string | null) => void;
}) {
  const t = useT();
  const [draft, setDraft] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const cap = allocation?.unit_cap ?? "";
  const issued = nav?.units_outstanding ?? "0";
  // Uncontrolled until the operator types: the field shows the stored cap, and switching
  // funds re-reads it rather than carrying the previous product's number across.
  const value = draft ?? cap;
  const parsed = toBaseUnits(value);
  const invalid = value.trim() !== "" && parsed <= 0n;
  const changed = value.trim() !== "" && parsed !== toBaseUnits(cap);
  const fraction = fractionOfCap(issued, cap);
  // A cap narrowed below what is already out is legal — it stops issuance without
  // touching a single minted unit — but it is worth saying out loud before saving.
  const belowIssued = changed && !invalid && parsed < toBaseUnits(issued);
  const nearCap = fraction >= 0.9;

  const save = async () => {
    if (!allocation) return;
    setSaving(true);
    onError(null);
    try {
      onSaved(await setAllocationUnitCap(allocation.service, value.trim()));
      setDraft(null);
    } catch (e) {
      onError(errorMessage(e, t));
    } finally {
      setSaving(false);
    }
  };

  if (!allocation) {
    return (
      <Card>
        <CardContent className="py-6 text-sm text-ink-soft">{t("admin.valuation.pickAFund")}</CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardContent className="space-y-5 py-6">
        <div className="space-y-2">
          <div className="flex flex-wrap items-baseline justify-between gap-2">
            <span className="text-sm text-ink-soft">{t("admin.valuation.unitsIssuedIn", { service: allocation.service })}</span>
            <span className={cn("text-sm font-semibold tabular-nums", nearCap ? "text-accent-warn" : "text-ink")}>
              {t("admin.valuation.issuedOfCap", { issued: compactUnits(issued), cap: compactUnits(cap) })}
            </span>
          </div>
          <div className="h-1.5 w-full overflow-hidden rounded-full bg-border">
            {/* Proportional, no minimum sliver — see `SupplyBar`. The exact issued figure
                sits directly above it. */}
            <div className={cn("h-full rounded-full", nearCap ? "bg-accent-warn" : "bg-accent-debug")} style={{ width: `${fraction * 100}%` }} />
          </div>
          {/* Two whole sentences rather than a shared " — …" tail: a suffix key would be a
              fragment no translator could place, and the loading branch reads differently
              from the figure branch in most languages. */}
          <p className="text-xs text-ink-soft">
            {nav ? t("admin.valuation.stillIssuable", { units: formatUnits(nav.remaining_capacity) }) : t("admin.valuation.loadingSupply")}
          </p>
        </div>

        <div className="flex flex-wrap items-end gap-3">
          <label className="flex w-56 flex-col gap-1.5">
            <span className="text-sm text-ink-soft">{t("admin.valuation.capUnits")}</span>
            <Input value={value} onChange={(e) => setDraft(e.target.value)} inputMode="decimal" placeholder="100000000" className="w-full" />
          </label>
          <Button type="button" className={cn(TEAL_CTA)} disabled={saving || invalid || !changed} onClick={save}>
            {saving ? <Loader2 className="size-4 animate-spin" /> : null}
            {t("admin.valuation.saveCap")}
          </Button>
          {/* i18n-max: 12 per verb — both Buttons are `shrink-0` in a wrapping row. */}
          {draft !== null && (
            <Button type="button" variant="outline" onClick={() => setDraft(null)}>
              {t("ui.reset")}
            </Button>
          )}
          <p className={cn("min-w-48 flex-1 text-xs", invalid ? "text-accent-error" : belowIssued ? "text-accent-warn" : "text-ink-soft")}>
            {invalid
              ? t("admin.valuation.capInvalid")
              : belowIssued
                ? t("admin.valuation.capBelowIssued", { n: compactUnits(issued) })
                : t("admin.valuation.capHint")}
          </p>

        </div>
      </CardContent>
    </Card>
  );
}
