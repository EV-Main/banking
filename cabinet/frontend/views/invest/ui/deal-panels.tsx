"use client";

// The two deal forms and the queue that follows a redemption. Extracted from the invest
// list when the product page arrived: a subscription has to behave identically wherever
// it is initiated from, and the surest way to guarantee that is one implementation.
//
// None of the three takes an `onDone`. The mutations they call name what they moved (see
// `entities/fund/model/fund-resource.ts`), so the page around them — and Home, and Wallet,
// and anything else showing a figure a deal touched — refreshes itself. A callback per
// panel was the same job done once per call site, which is the version that goes stale.

import { useT } from "@evinvest/i18n/react";
import { ArrowDownToLine, Clock, Loader2, Sparkles, TriangleAlert, X } from "lucide-react";
import { useState } from "react";

import { Alert, AlertDescription, AlertTitle, Button, Input } from "@evinvest/uikit";

import { cancelRedemption, submitRedeem, submitSubscribe } from "@/entities/fund/model/fund-resource";
import type { FundNav, Position, Redemption } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { TipAnchor } from "@/shared/tips";
import { Panel, PanelPresence } from "@/shared/ui/motion";
import { formatUnits, formatUsdt, fromBaseUnits, toBaseUnits } from "@/views/invest/lib/format";
import { cashForUnits, unitsForCash } from "@/views/invest/lib/product";
import { TEAL_CTA } from "@/views/invest/ui/atoms";

export function SubscribePanel({ service, nav }: { service: string; nav: FundNav | null }) {
  const t = useT();
  const [amount, setAmount] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [done, setDone] = useState<{ units?: string; nav?: string } | null>(null);

  // Exact preview, floored the same way the hub floors it — so "0 units" is visible here
  // instead of arriving as a rejection.
  const preview = unitsForCash(amount, nav?.nav);
  const dust = toBaseUnits(amount) > 0n && preview === 0n;
  // The supply cap, checked before the submit rather than after it. `remaining_capacity`
  // already counts in-flight mints, so this is the same figure the hub will gate on.
  const headroom = nav ? toBaseUnits(nav.remaining_capacity) : null;
  const overCap = preview !== null && headroom !== null && preview > headroom;

  const submit = async () => {
    setSubmitting(true);
    setError(null);
    setDone(null);
    try {
      const receipt = await submitSubscribe({ service, amount });
      setDone({ units: receipt.units, nav: receipt.nav });
      setAmount("");
    } catch (e) {
      // The error itself: `errorMessage` resolves its `code` in the reader's locale.
      setError(e);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="space-y-3 rounded-lg border border-border bg-secondary p-4">
      {/* The receipt and the failure occupy the same slot and replace one another, so
          they share a presence boundary: retrying after an error swaps the panel in
          place instead of collapsing the form and re-expanding it. */}
      <PanelPresence>
        {done && (
          <Panel key="receipt" from="bottom">
            <Alert>
              <Sparkles className="size-4 text-positive" />
              <AlertTitle>{t("invest.subscribeReceived", "Subscription received")}</AlertTitle>
              <AlertDescription>{t("invest.subscribeReceiptBody", "Minted {n, plural, one {{units} unit} other {{units} units}} at {nav} USDT NAV — your position updates shortly.", { n: Number(done.units ?? 0), units: formatUnits(done.units), nav: formatUsdt(done.nav) })}</AlertDescription>
            </Alert>
          </Panel>
        )}
        {!!error && (
          <Panel key="error" from="bottom">
            <Alert variant="destructive">
              <TriangleAlert className="size-4" />
              <AlertTitle>{t("invest.subscribeFailed", "Subscription failed")}</AlertTitle>
              <AlertDescription>{errorMessage(error, t)}</AlertDescription>
            </Alert>
          </Panel>
        )}
      </PanelPresence>

      <div className="flex flex-wrap items-end gap-3">
        <label className="flex min-w-48 flex-1 flex-col gap-1.5">
          <span className="flex items-center gap-1.5 text-sm">
            {t("admin.revenue.amountUsdt", "Amount (USDT)")}
            <TipAnchor anchor="invest.subscribe.amount" />
          </span>
          <Input value={amount} onChange={(e) => setAmount(e.target.value)} inputMode="decimal" placeholder="0.00" className="w-full" />
        </label>
        <Button type="button" className={cn(TEAL_CTA)} disabled={submitting || preview === null || preview === 0n || overCap} onClick={submit}>
          {submitting ? <Loader2 className="size-4 animate-spin" /> : <Sparkles className="size-4" />}
          {t("invest.subscribe", "Subscribe")}
        </Button>
      </div>

      <p className={cn("text-xs", dust || overCap ? "text-accent-error" : "text-ink-soft")}>
        {dust
          ? t("invest.dustHint", "Too small to buy a whole unit at {nav} — increase the amount.", { nav: formatUsdt(nav?.nav) })
          : overCap
            ? t("invest.overCapHint", "Only {n, plural, one {{units} unit is} other {{units} units are}} left before this fund reaches its cap — reduce the amount.", { n: Number(fromBaseUnits(headroom ?? 0n)), units: formatUnits(fromBaseUnits(headroom ?? 0n)) })
            : preview !== null
              ? t("invest.buysUnits", "Buys {n, plural, one {{units} unit} other {{units} units}} at {nav} USDT per unit.", { n: Number(fromBaseUnits(preview)), units: formatUnits(fromBaseUnits(preview)), nav: formatUsdt(nav?.nav) })
              : t("invest.subscribeIdleHint", "Units are priced at the current NAV; the unit count is fixed at purchase and does not grow.")}
      </p>
    </div>
  );
}

export function RedeemPanel({ service, position, nav }: { service: string; position: Position; nav: FundNav | null }) {
  const t = useT();
  const [units, setUnits] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [done, setDone] = useState<Redemption | null>(null);

  const estimate = cashForUnits(units, nav?.nav);
  const overdraw = toBaseUnits(units) > toBaseUnits(position.units);

  const submit = async () => {
    setSubmitting(true);
    setError(null);
    setDone(null);
    try {
      const redemption = await submitRedeem({ service, units });
      setDone(redemption);
      setUnits("");
    } catch (e) {
      // The error itself: `errorMessage` resolves its `code` in the reader's locale.
      setError(e);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="space-y-3 rounded-lg border border-border bg-secondary p-4">
      <PanelPresence>
        {done && (
          <Panel key="receipt" from="bottom">
            <Alert>
              <Clock className="size-4 text-accent-warn" />
              <AlertTitle>{(done.state === "completed" ? t("invest.redeemCompleted", "Redemption completed") : t("invest.redeemQueued", "Redemption queued"))}</AlertTitle>
              <AlertDescription>
                {done.state === "completed"
                  ? t("invest.redeemReceiptCompleted", "{n, plural, one {{units} unit} other {{units} units}} redeemed for {cash} USDT at {nav} USDT NAV.", { n: Number(done.units ?? 0), units: formatUnits(done.units), cash: formatUsdt(done.cash), nav: formatUsdt(done.nav) })
                  : t("invest.redeemReceiptQueued", "{n, plural, one {{units} unit} other {{units} units}} reserved — queued until the fund tops up, then priced at the settle NAV.", { n: Number(done.units ?? 0), units: formatUnits(done.units) })}
              </AlertDescription>
            </Alert>
          </Panel>
        )}
        {!!error && (
          <Panel key="error" from="bottom">
            <Alert variant="destructive">
              <TriangleAlert className="size-4" />
              <AlertTitle>{t("invest.redeemFailed", "Redemption failed")}</AlertTitle>
              <AlertDescription>{errorMessage(error, t)}</AlertDescription>
            </Alert>
          </Panel>
        )}
      </PanelPresence>

      {/* The one genuinely surprising rule of this product, stated on the action itself. */}
      <p className="flex items-start gap-1.5 text-xs text-accent-warn">
        <Clock className="mt-0.5 size-3.5 shrink-0" />
        <span>
          {t("invest.redeemTimingNote", "Your units are reserved the moment you submit, but the cash is priced when the redemption settles — not now. A figure shown here is an estimate at today's NAV.")}
          <TipAnchor anchor="invest.redeem.queue" />
        </span>
      </p>

      <div className="flex flex-wrap items-end gap-3">
        <label className="flex min-w-48 flex-1 flex-col gap-1.5">
          <span className="flex items-center justify-between text-sm">
            <span className="flex items-center gap-1.5">
              {t("invest.unitsToRedeem", "Units to redeem")}
              <TipAnchor anchor="invest.redeem.units" />
            </span>
            <button type="button" className="text-xs text-accent-debug hover:underline" onClick={() => setUnits(position.units ?? "0")}>
              {t("ui.max", "Max")}
            </button>
          </span>
          <Input value={units} onChange={(e) => setUnits(e.target.value)} inputMode="decimal" placeholder="0.00" className="w-full" />
        </label>
        <Button type="button" variant="outline" disabled={submitting || estimate === null || overdraw} onClick={submit}>
          {submitting ? <Loader2 className="size-4 animate-spin" /> : <ArrowDownToLine className="size-4" />}
          {t("invest.redeem", "Redeem")}
        </Button>
      </div>

      <p className={cn("text-xs", overdraw ? "text-accent-error" : "text-ink-soft")}>
        {overdraw
          ? t("invest.youHoldUnits", "You hold {n, plural, one {{units} unit} other {{units} units}}.", { n: Number(position.units ?? 0), units: formatUnits(position.units) })
          : estimate !== null
            ? t("invest.redeemEstimate", "≈ {amount} USDT at today's NAV — the settle price may differ.", { amount: formatUsdt(fromBaseUnits(estimate)) })
            : t("invest.unitsHeld", "{n, plural, one {{units} unit} other {{units} units}} held.", { n: Number(position.units ?? 0), units: formatUnits(position.units) })}
      </p>
    </div>
  );
}

/** Queued redemptions for this product, inline — they belong to the fund they came from,
 *  not to a separate activity list the holder has to go and find. */
export function QueuedList({ items }: { items: Redemption[] }) {
  const t = useT();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);

  const cancel = async (id: string) => {
    setBusy(id);
    setError(null);
    try {
      await cancelRedemption(id);
    } catch (e) {
      // The error itself: `errorMessage` resolves its `code` in the reader's locale.
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="space-y-2 rounded-lg border border-accent-warn/30 bg-accent-warn/5 p-3">
      <p className="flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide text-accent-warn">
        {t("invest.awaitingSettlement", "Awaiting settlement")}
        <TipAnchor anchor="invest.activity.status" />
      </p>
      {!!error && <p className="text-xs text-accent-error">{errorMessage(error, t)}</p>}
      {items.map((r) => (
        <div key={r.id ?? ""} className="flex items-center justify-between gap-3 text-sm">
          <span>
            <span className="font-medium">{t("dash.unitsAmount", "{n, plural, one {{units} unit} other {{units} units}}", { n: Number(r.units ?? 0), units: formatUnits(r.units) })}</span>{" "}
            <span className="text-ink-soft">{t("invest.reservedPricedAtSettle", "reserved, priced at settle")}</span>
          </span>
          <span className="flex items-center gap-2">
            <Button type="button" variant="outline" size="sm" disabled={busy === (r.id ?? "")} onClick={() => cancel(r.id ?? "")}>
              {busy === (r.id ?? "") ? <Loader2 className="size-3 animate-spin" /> : <X className="size-3" />}
              {t("ui.cancel", "Cancel")}
            </Button>
            <TipAnchor anchor="invest.activity.cancel" />
          </span>
        </div>
      ))}
    </div>
  );
}
