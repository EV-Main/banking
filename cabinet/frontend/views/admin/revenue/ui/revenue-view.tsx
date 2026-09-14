"use client";

// Admin console — the fund's OWN money: what it earned, and PROPOSING that it be paid out.
//
// The screen's whole job is to make one distinction unmistakable, because it is the one
// an operator could otherwise get wrong with real consequences: this page concerns company
// revenue (retained withdrawal fees + the settled 2-and-20), never client balances and
// never the fund's seed capital. Those are separate ledger claims that this surface
// cannot reach at all — the cap below is the money plane's, enforced against the revenue
// claim's available balance with TigerBeetle's non-negative flag underneath. The form's
// own cap is a courtesy that stops a typo early, not the control.
//
// ── This form no longer moves money ──────────────────────────────────────────
//
// It used to call `BalanceService.RequestRevenuePayout`, which paid out on one operator's
// click. That RPC is now closed and answers FAILED_PRECONDITION: while it worked, a single
// admin could empty the fund's revenue and the consilium was decorative (docs/CONSILIUM.md).
//
// So this form opens a CONSILIUM. Submitting proposes a payout; a majority of the owners
// then confirm it from their own mailboxes, and only then does any money move. Every word
// on this surface has to carry that, because the failure it invites is an operator reading
// "requested" as "sent", closing the tab, and believing the fund has paid — when in fact
// nothing will happen at all unless enough owners answer their email within 72 hours.
// Nothing here says queued, submitted, shipped or in flight; the receipt says proposed, and
// says what has to happen next.
//
// The payout history below is unchanged and still lists withdrawals: a consilium that
// carries executes as an ordinary payout, and that is where it appears.

import { Banknote, Clock, Loader2, MailWarning, ShieldAlert, Users } from "lucide-react";
import { useMemo, useState } from "react";

import { useLocale, useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle, Input, Progress, Skeleton } from "@evinvest/uikit";

import { cancelRevenuePayout } from "@/entities/admin/api/admin-client";
import { fundRevenueResource, revenuePayoutsResource } from "@/entities/admin/model/admin-resource";
import { openRevenuePayout, ownersResource } from "@/entities/governance/model/governance-resource";
import type { RevenuePayout, RevenueRail } from "@/shared/contracts/admin";
import type { Consilium } from "@/shared/contracts/governance";
import { errorMessage } from "@/shared/lib/api-client";
import { TAG } from "@/shared/lib/cache-tags";
import { classifyConsiliumRefusal, coolingOffLiftsAt, type ConsiliumRefusal } from "@/shared/lib/consilium-refusal";
import { expiresIn, formatMoment } from "@/shared/lib/datetime";
import { formatExactUsdt } from "@/shared/lib/money";
import { revalidateTag } from "@/shared/lib/resource";
import { useResource } from "@/shared/lib/resource";
import { Link } from "@/shared/ui/cabinet-link";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { networkLabel } from "@/shared/lib/rail";
import { NetworkMark } from "@/shared/ui/icons/networks";
import { amount as formatAmount, formatUsd, railLabel, stateLabel } from "@/views/admin/lib/format";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

/** In flight — the operator can still act on these; the rest are history. */
const OPEN_STATES = new Set(["queued", "processing"]);

export function RevenueView() {
  const t = useT();
  const revenue = useResource(fundRevenueResource);
  const payouts = useResource(revenuePayoutsResource);
  // The roster is read for one fact: whether the fund can reach a payout threshold at all.
  // Cheap — it is the same cached read the consilium page uses, shared between them.
  const owners = useResource(ownersResource);

  const [network, setNetwork] = useState<string>("");
  const [address, setAddress] = useState("");
  const [amount, setAmount] = useState("");
  const [memo, setMemo] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  /** The consilium this screen just opened — a proposal, not a payment. */
  const [proposed, setProposed] = useState<Consilium | null>(null);
  /** A refusal we have specific words for, with the deadline resolved at arrival. */
  const [refusal, setRefusal] = useState<{ detail: ConsiliumRefusal; liftsAt: string | null } | null>(null);

  const data = revenue.data ?? null;
  const rails = data?.rails ?? [];
  // The first configured rail is the default, so the form is usable without a choice.
  const rail = rails.find((r) => r.network === network) ?? rails[0];
  const history = payouts.data?.withdrawals ?? null;
  const error = actionError ?? (data || !revenue.error ? null : errorMessage(revenue.error, t));

  const available = Number(data?.available ?? "0");
  const requested = Number(amount);
  const minimum = Number(rail?.minimum ?? "0");
  // Mirrors the hub's own refusals so the operator hears about a bad amount before
  // spending a round trip on it. The hub still decides.
  const amountProblem = useMemo(() => {
    if (!amount.trim()) return null;
    if (!Number.isFinite(requested) || requested <= 0) return t("admin.revenue.err.enterAmount", "Enter an amount.");
    if (rail && requested < minimum) return t("admin.revenue.err.belowMinimum", "Below the {min} minimum for this rail.", { min: formatUsd(rail.minimum) });
    if (requested > available) return t("admin.revenue.err.overAvailable", "Only {available} of earned revenue is available.", { available: formatUsd(data?.available) });
    return null;
  }, [amount, requested, rail, minimum, available, data?.available, t]);

  const canSubmit = Boolean(rail) && address.trim().length > 0 && amount.trim().length > 0 && !amountProblem && busy === null;
  // Beyond `instant` the payout is accepted and queued until the treasury is topped up —
  // say so before the click, not after, so a queued row is never read as a failure.
  const willQueue = Boolean(rail) && Number.isFinite(requested) && requested > Number(rail?.instant ?? "0");

  // A payout debits a claim and joins the operator withdrawal queue, so it moves three
  // facts, not one. Naming all three keeps the treasury and queue screens in step.
  const settle = async () => {
    revalidateTag(TAG.adminRevenue, TAG.adminQueue, TAG.adminTreasury);
    await Promise.all([revenue.refresh(), payouts.refresh()]);
  };

  // Opening a consilium moves NO money, so it invalidates none of the money reads. What it
  // changes is the governance state, and `openRevenuePayout` names that tag itself.
  const submit = async () => {
    if (!rail) return;
    setBusy("request");
    setActionError(null);
    setRefusal(null);
    try {
      const trimmedMemo = memo.trim();
      const consilium = await openRevenuePayout({
        network: rail.network,
        address: address.trim(),
        amount: amount.trim(),
        // Omitted rather than sent empty: the memo is part of the canonical payload the
        // `payload_hash` is computed over, so an empty string and an absent field are not
        // the same request.
        ...(trimmedMemo ? { memo: trimmedMemo } : {}),
      });
      setProposed(consilium);
      setAddress("");
      setAmount("");
      setMemo("");
      setConfirming(false);
    } catch (e) {
      const detail = classifyConsiliumRefusal(e);
      if (detail) {
        // Resolved once, here: the plane sends a duration, and a duration re-based on every
        // render drifts away from the deadline it describes.
        setRefusal({ detail, liftsAt: detail.kind === "cooling-off" ? coolingOffLiftsAt(detail) : null });
      } else {
        setActionError(errorMessage(e, t));
      }
    } finally {
      setBusy(null);
    }
  };

  const cancel = async (id: string) => {
    setBusy(id);
    setActionError(null);
    try {
      await cancelRevenuePayout(id);
      await settle();
    } catch (e) {
      setActionError(errorMessage(e, t));
    } finally {
      setBusy(null);
    }
  };

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader eyebrow={t("admin.eyebrow.administer", "Administer")} title={t("nav.revenue", "Fund revenue")} subtitle={t("admin.revenue.subtitle", "What the fund earned — fees and the settled 2-and-20 — and proposing that it be paid out")} />

      {error && <ResourceError message={error} />}

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.revenue.earned", "Earned")}</p>
        <div className="grid gap-4 sm:grid-cols-3">
          <MoneyCard label={t("admin.revenue.earnedTotal", "Earned · total")} value={data?.earned} hint={t("admin.revenue.earnedTotalHint", "fees + settled 2-and-20")} loading={!data} />
          <MoneyCard label={t("admin.revenue.availableToPayOut", "Available to pay out")} value={data?.available} hint={t("admin.revenue.availableHint", "free of in-flight payouts")} loading={!data} emphasis />
          <MoneyCard label={t("admin.revenue.pendingPayout", "Pending payout")} value={data?.pending_payout} hint={t("admin.revenue.pendingHint", "queued + in-flight")} loading={!data} />
        </div>
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.revenue.ownMoneyNote", "This is the fund's own money. Client balances and the fund's seed capital are separate ledger claims and are not included here — and cannot be reached from this screen.")}</p>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.revenue.propose", "Propose a payout")}</p>
        <Card>
          <CardContent className="space-y-4 py-5">
            {/* Stated before the form, not after it: an operator filling this in should know
                what the button does before they reach it, not discover it in the receipt. */}
            <p className="text-xs text-ink-soft">{t("admin.revenue.consiliumNote", "Paying the fund's revenue out is not one person's decision. Submitting here proposes a payout and emails every other owner a link to confirm it; the money moves only once enough of them have.")}</p>

            {owners.data?.below_payout_floor && (
              // The threshold is `floor(N/2)+1` of ALL owners, so below three it can never
              // be met and the plane refuses to open at all. Saying so here saves the
              // operator filling in a form that cannot succeed.
              <div className="flex items-start gap-3 rounded-lg border border-accent-warn/40 bg-accent-warn/10 px-3.5 py-3">
                <Users className="mt-0.5 size-4 shrink-0 text-accent-warn" />
                <div className="min-w-0 space-y-1">
                  <p className="text-sm font-semibold text-ink">{t("admin.revenue.refusal.floorTitle", "This fund cannot authorise a payout")}</p>
                  <p className="text-sm leading-relaxed text-ink">
                    {t("admin.revenue.floorBody", "A payout needs more than half of the owners to agree, and with {n, plural, one {# owner} other {# owners}} that bar cannot be met. A proposal made now will be refused; adding owners restores it.", { n: owners.data.items.length })}
                  </p>
                </div>
              </div>
            )}

            {refusal && <RefusalNotice refusal={refusal.detail} liftsAt={refusal.liftsAt} />}

            {proposed && <ProposedReceipt consilium={proposed} onDismiss={() => setProposed(null)} />}

            {/* The inner spacing lives on `Settled`: it wraps its children in a div of its
                own, so a `space-y` on CardContent has exactly one child to act on here and
                never reaches the form rows inside. */}
            <Settled className="space-y-4" loading={!data} skeleton={<Skeleton className="h-40 w-full" />}>
              {!data ? null : rails.length === 0 ? (
                <Empty className="border md:p-6">
                  <EmptyHeader>
                    <EmptyMedia variant="icon">
                      <Banknote />
                    </EmptyMedia>
                    <EmptyTitle>{t("admin.revenue.noRail", "No rail is configured")}</EmptyTitle>
                    <EmptyDescription>{t("admin.revenue.noRailHint", "A payout ships on a chain rail with a running watcher. Configure one to enable payouts.")}</EmptyDescription>
                  </EmptyHeader>
                </Empty>
              ) : (
                <>
                  <div className="space-y-2">
                    <p className="text-xs text-ink-soft">{t("admin.rail", "Rail")}</p>
                    <div className="flex flex-wrap gap-2">
                      {rails.map((r) => (
                        <RailChip key={r.network} rail={r} selected={r.network === rail?.network} onSelect={() => setNetwork(r.network)} />
                      ))}
                    </div>
                  </div>

                  <div className="grid gap-4 sm:grid-cols-2">
                    <label className="block space-y-1.5">
                      <span className="block text-xs text-ink-soft">{t("admin.revenue.destinationAddress", "Destination address")}</span>
                      <Input
                        value={address}
                        onChange={(e) => {
                          setAddress(e.target.value);
                          setConfirming(false);
                        }}
                        placeholder={t("admin.revenue.placeholder.address", "The wallet that receives the payout")}
                        spellCheck={false}
                        className="font-mono-tech text-xs"
                      />
                    </label>
                    <label className="block space-y-1.5">
                      <span className="block text-xs text-ink-soft">{t("admin.revenue.amountUsdt", "Amount (USDT)")}</span>
                      <Input
                        value={amount}
                        onChange={(e) => {
                          setAmount(e.target.value);
                          setConfirming(false);
                        }}
                        inputMode="decimal"
                        placeholder={rail ? t("admin.revenue.placeholder.minAmount", "min {n}", { n: formatAmount(rail.minimum) }) : "0.00"}
                        className="tabular-nums"
                      />
                    </label>
                  </div>

                  <label className="block space-y-1.5">
                    <span className="block text-xs text-ink-soft">{t("admin.revenue.memoOptional", "Memo (optional)")}</span>
                    <Input
                      value={memo}
                      onChange={(e) => {
                        setMemo(e.target.value);
                        setConfirming(false);
                      }}
                      placeholder={t("admin.revenue.placeholder.memo", "Required by some rails to credit the destination")}
                      spellCheck={false}
                      className="font-mono-tech text-xs"
                    />
                    <span className="block text-xs text-ink-soft">{t("admin.revenue.memoHint", "Part of what the owners are asked to approve, and covered by the request fingerprint — it cannot be changed afterwards.")}</span>
                  </label>

                  {amountProblem && <p className="text-xs text-accent-error">{amountProblem}</p>}
                  {!amountProblem && willQueue && (
                    // Reworded for the consilium: this describes what happens IF the owners
                    // approve, not what happens on submit. Nothing is queued by proposing.
                    <p className="text-xs text-accent-warn">{t("admin.revenue.willQueueIfApproved", "Above the {instant} that ships immediately on this rail — if the owners approve it, the remainder is queued until the treasury is topped up.", { instant: formatUsd(rail?.instant) })}</p>
                  )}

                  {/* A second, deliberate click. The terms are restated because they are
                      what the owners will be emailed and what `payload_hash` is computed
                      over — the request is immutable once open, so a typo here means
                      cancelling and starting again, not editing (docs/CONSILIUM.md). */}
                  {confirming ? (
                    <div className="space-y-2 rounded-lg border border-accent-warn/40 bg-accent-warn/5 p-3">
                      {/* One sentence, one key: which order the amount, the rail and the
                          address fall in is a per-language decision, so the three are ICU
                          arguments rather than spans the sentence is cut around. The address
                          is 40-plus unbroken characters, so `break-words` stays on the
                          paragraph — without it the panel widens past the phone.
                          The amount is the exact wire decimal, because it is the figure the
                          owners will be asked to authorize and the one the hash covers. */}
                      <p className="text-sm break-words">
                        {t("admin.revenue.proposeSentence", "Propose paying {amount} on {network} to {address}?", {
                          amount: `${formatExactUsdt(amount.trim())} USDT`,
                          network: (rail?.network ?? "").toUpperCase(),
                          address: address.trim(),
                        })}
                      </p>
                      {memo.trim() && (
                        <p className="text-sm break-words">
                          {t("admin.revenue.proposeMemoLine", "With memo: {memo}", { memo: memo.trim() })}
                        </p>
                      )}
                      <p className="text-xs text-ink-soft">{t("admin.revenue.proposeNote", "The terms are fixed once proposed. Every owner is emailed exactly these, and they are checked again before any money moves — so changing anything means cancelling and starting over, and votes are not carried across.")}</p>
                      {/* i18n-max: 16 per verb — two `shrink-0` Buttons in a ≤400px panel. */}
                      <div className="flex gap-2">
                        <Button type="button" size="sm" disabled={busy !== null} onClick={submit}>
                          {busy === "request" ? <Loader2 className="size-4 animate-spin" /> : null}
                          {t("admin.revenue.confirmPropose", "Propose payout")}
                        </Button>
                        <Button type="button" size="sm" variant="outline" disabled={busy !== null} onClick={() => setConfirming(false)}>
                          {t("ui.back", "Back")}
                        </Button>
                      </div>
                    </div>
                  ) : (
                    <Button type="button" size="sm" disabled={!canSubmit} onClick={() => setConfirming(true)}>
                      {t("admin.revenue.reviewPropose", "Review proposal")}
                    </Button>
                  )}
                </>
              )}
            </Settled>
          </CardContent>
        </Card>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.revenue.payouts", "Payouts")}</p>
        <Card>
          <CardContent className="p-0">
            <Settled
              loading={!history}
              skeleton={
                <div className="p-6">
                  <Skeleton className="h-24 w-full" />
                </div>
              }
            >
              {!history ? null : history.length === 0 ? (
                <div className="p-8">
                  <Empty className="border md:p-6">
                    <EmptyHeader>
                      <EmptyMedia variant="icon">
                        <Banknote />
                      </EmptyMedia>
                      <EmptyTitle>{t("admin.revenue.noPayouts", "No payouts yet")}</EmptyTitle>
                      <EmptyDescription>{t("admin.revenue.noPayoutsHintConsilium", "Revenue the fund has earned stays on the ledger until the owners approve a payout. Approved payouts appear here once they execute.")}</EmptyDescription>
                    </EmptyHeader>
                  </Empty>
                </div>
              ) : (
                // Five columns do not fit a phone. The table scrolls inside its own box rather
                // than making the whole page scroll sideways.
                <div className="overflow-x-auto">
                  <table className="w-full min-w-140 text-sm">
                    <thead>
                      {/* i18n-max: 14 per header — the table scrolls inside `min-w-140`, so a
                          long header widens the scroll rather than clipping a cell. */}
                      <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-ink-soft">
                        <th className="px-5 py-3 font-medium">{t("ui.destination", "Destination")}</th>
                        <th className="px-5 py-3 font-medium">{t("ui.amount", "Amount")}</th>
                        <th className="px-5 py-3 font-medium">{t("admin.col.state", "State")}</th>
                        <th className="px-5 py-3 font-medium">{t("admin.col.transaction", "Transaction")}</th>
                        <th className="px-5 py-3 text-right font-medium">{t("admin.col.actions", "Actions")}</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y divide-border">
                      {history.map((payout) => (
                        <PayoutRow key={payout.id} payout={payout} busy={busy === payout.id} onCancel={() => cancel(payout.id)} />
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </Settled>
          </CardContent>
        </Card>
        <p className="max-w-3xl text-xs text-ink-soft">{t("admin.revenue.footnote", "A payout runs the same saga as a user withdrawal, so it also appears on the Withdrawals queue for dispatch and settle. Only a still-queued payout can be cancelled — once processing, a broadcast may have landed and voiding it would double-pay.")}</p>
      </StaggerItem>
    </AdminScreen>
  );
}

/**
 * What just happened, said accurately.
 *
 * This is the single most important piece of copy on the screen. The operator has clicked a
 * button on a money surface and the natural reading of any receipt is "it is done" — so this
 * one leads with the word PROPOSED, states the tally as `0 of N` rather than implying
 * progress, gives the deadline, and says in plain words that nothing moves until the owners
 * answer. It deliberately offers no "view payout" affordance, because there is no payout: it
 * links to the consilium, which is the only place the live tally exists.
 *
 * The tally shown here is the one the server returned when it opened the request, and it is
 * never updated in place — the live count belongs to the consilium page, behind the link
 * (docs/CONSILIUM.md, policy 21).
 */
function ProposedReceipt({ consilium, onDismiss }: { consilium: Consilium; onDismiss: () => void }) {
  const t = useT();
  const locale = useLocale();
  const threshold = consilium.threshold ?? 0;
  const approvals = consilium.approvals ?? 0;
  const progress = threshold > 0 ? Math.min(100, Math.round((approvals / threshold) * 100)) : 0;

  return (
    <div className="space-y-3 rounded-lg border border-positive/40 bg-positive/10 p-4" role="status">
      <div className="space-y-1">
        <p className="text-sm font-semibold text-ink">{t("admin.revenue.proposedTitle", "Proposed — no money has moved")}</p>
        <p className="text-sm leading-relaxed text-ink">
          {t("admin.revenue.proposedBody", "{threshold} of the fund's {owners, plural, one {# owner} other {# owners}} must confirm this from their own mailbox, and each has been emailed a link. Nothing is paid out until enough of them answer.", { threshold, owners: consilium.owner_count ?? 0 })}
        </p>
      </div>

      <div className="space-y-1.5">
        <p className="text-sm font-medium tabular-nums text-ink">
          {t("admin.revenue.proposedTally", "{approvals} of {threshold, plural, one {# approval} other {# approvals}} so far", { approvals, threshold })}
        </p>
        {/* The sentence above states the tally; a second, unlabelled progressbar in the
            accessibility tree would only repeat it. */}
        <Progress value={progress} className="h-1.5" aria-hidden />
      </div>

      <p className="text-xs tabular-nums text-ink-soft">
        {t("admin.revenue.proposedExpires", "Expires {at} · {left}", {
          at: formatMoment(consilium.expires_at, locale),
          left: expiresIn(consilium.expires_at, t),
        })}
      </p>

      <div className="flex flex-wrap gap-2">
        <Button asChild size="sm" variant="outline">
          <Link href="/consilium">{t("admin.revenue.proposedOpenConsilium", "Open the consilium")}</Link>
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onDismiss}>
          {t("ui.close", "Close")}
        </Button>
      </div>
    </div>
  );
}

/**
 * A refusal the money plane raised on purpose, in words that say what to do about it.
 *
 * Two of the three arrive with the same status and are told apart by their message
 * (`shared/lib/consilium-refusal.ts`); all three are conditions rather than faults, so none
 * is styled as an error. The cooling-off one shows a clock time rather than the duration the
 * backend sent, because a duration is stale the moment it is rendered and an operator
 * planning around it has to do arithmetic on it.
 */
function RefusalNotice({ refusal, liftsAt }: { refusal: ConsiliumRefusal; liftsAt: string | null }) {
  const t = useT();
  const locale = useLocale();

  const { icon, title, body } =
    refusal.kind === "mail-not-configured"
      ? {
          icon: <MailWarning className="mt-0.5 size-4 shrink-0 text-accent-warn" />,
          title: t("admin.revenue.refusal.mailTitle", "Governance mail is not configured"),
          body: t("admin.revenue.refusal.mailBody", "Owners receive their approval link by email, and no mailer is wired — so a proposal opened now could never be voted on and would simply expire. This has to be configured before the fund can pay itself out."),
        }
      : refusal.kind === "cooling-off"
        ? {
            icon: <Clock className="mt-0.5 size-4 shrink-0 text-accent-warn" />,
            title: t("admin.revenue.refusal.coolingTitle", "Payouts are paused while the roster settles"),
            // Without a parseable deadline the condition is still named — better than a
            // sentence with a hole in it where the time should be.
            body: liftsAt
              ? t("admin.revenue.refusal.coolingBody", "The list of owners changed within the last 48 hours. Payouts resume at {at}. The pause is deliberate: it keeps a change of ownership and a payout as two separate, visible events rather than one motion.", { at: formatMoment(liftsAt, locale) })
              : t("admin.revenue.refusal.coolingBodyNoTime", "The list of owners changed within the last 48 hours, and payouts stay paused until that period lifts. The pause is deliberate: it keeps a change of ownership and a payout as two separate, visible events rather than one motion."),
          }
        : {
            icon: <ShieldAlert className="mt-0.5 size-4 shrink-0 text-accent-warn" />,
            title: t("admin.revenue.refusal.floorTitle", "This fund cannot authorise a payout"),
            body:
              refusal.ownerCount === null
                ? t("admin.revenue.refusal.floorBodyNoCount", "A payout needs more than half of the owners to agree, and this fund has too few owners for that bar to be met. Adding owners restores it.")
                : t("admin.revenue.refusal.floorBody", "A payout needs more than half of the owners to agree, and with {n, plural, one {# owner} other {# owners}} that bar can never be met. Adding owners restores it.", { n: refusal.ownerCount }),
          };

  return (
    <div className="flex items-start gap-3 rounded-lg border border-accent-warn/40 bg-accent-warn/10 px-3.5 py-3" role="status">
      {icon}
      <div className="min-w-0 space-y-1">
        <p className="text-sm font-semibold text-ink">{title}</p>
        <p className="text-sm leading-relaxed text-ink">{body}</p>
      </div>
    </div>
  );
}

function MoneyCard({ label, value, hint, loading, emphasis }: { label: string; value: string | undefined; hint: string; loading: boolean; emphasis?: boolean }) {
  return (
    <Card>
      <CardContent className="space-y-1 py-5">
        <p className="text-xs text-ink-soft">{label}</p>
        {loading ? (
          <Skeleton className="mt-1 h-8 w-28" />
        ) : (
          // One step for every figure; the payable one carries the difference in colour,
          // not in size, so the row keeps a single baseline.
          <p className={emphasis ? "text-3xl font-semibold tabular-nums text-positive" : "text-3xl font-semibold tabular-nums"}>{formatUsd(value)}</p>
        )}
        {!loading && <p className="text-xs text-positive">{hint}</p>}
      </CardContent>
    </Card>
  );
}

function RailChip({ rail, selected, onSelect }: { rail: RevenueRail; selected: boolean; onSelect: () => void }) {
  const t = useT();
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-pressed={selected}
      className={`rounded-lg border px-3 py-2 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring ${
        selected ? "border-primary bg-primary/10" : "border-border hover:bg-ink/5"
      }`}
    >
      <span className="flex items-center gap-1.5 text-xs font-medium">
        <NetworkMark network={rail.network} className="size-3.5 shrink-0" />
        {railLabel(rail.network, t)}
      </span>
      <span className="block text-xs tabular-nums text-ink-soft">{t("admin.revenue.instantSuffix", "{amount} instant", { amount: formatUsd(rail.instant) })}</span>
    </button>
  );
}

function PayoutRow({ payout, busy, onCancel }: { payout: RevenuePayout; busy: boolean; onCancel: () => void }) {
  const t = useT();
  const open = OPEN_STATES.has(payout.state);
  return (
    <tr>
      <td className="px-5 py-3">
        {/* Same change as the withdrawals queue: the shared short mark, not the raw
            wire id. */}
        <p className="flex items-center gap-1.5 text-xs text-ink-soft">
          <NetworkMark network={payout.network} className="size-3.5 shrink-0" />
          {networkLabel(payout.network)}
        </p>
        <p className="font-mono-tech text-xs" title={payout.address}>
          {shortAddr(payout.address)}
        </p>
      </td>
      <td className="px-5 py-3 tabular-nums">{formatUsd(payout.amount)}</td>
      <td className="px-5 py-3">
        <span className={stateTone(payout.state)}>{stateLabel(payout.state, t)}</span>
      </td>
      <td className="px-5 py-3 font-mono-tech text-xs text-ink-soft" title={payout.tx_ref || undefined}>
        {payout.tx_ref ? shortAddr(payout.tx_ref) : "—"}
      </td>
      <td className="px-5 py-3">
        <div className="flex justify-end">
          {payout.state === "queued" ? (
            <Button type="button" variant="outline" size="sm" disabled={busy} onClick={onCancel}>
              {busy ? <Loader2 className="size-4 animate-spin" /> : null}
              {t("ui.cancel", "Cancel")}
            </Button>
          ) : (
            <span className="text-xs text-ink-soft">{open ? t("admin.revenue.inFlight", "In flight") : "—"}</span>

          )}
        </div>
      </td>
    </tr>
  );
}

function stateTone(state: string): string {
  switch (state) {
    case "completed":
      return "text-positive";
    case "queued":
    case "processing":
      return "text-accent-warn";
    case "failed":
      return "text-accent-error";
    default:
      return "text-ink-soft";
  }
}

function shortAddr(value: string): string {
  return value.length > 18 ? `${value.slice(0, 8)}…${value.slice(-6)}` : value;
}
