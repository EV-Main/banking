"use client";

import { useLocale, useT } from "@evinvest/i18n/react";

import { Clock, TriangleAlert } from "lucide-react";
import { type ReactNode, useId, useState } from "react";

import { Button, Skeleton, Spinner } from "@evinvest/uikit";

import { isUnverified } from "@/entities/user/lib/kyc";
import { profileResource } from "@/entities/user/model/profile-resource";
import { submitWithdrawal, walletResource } from "@/entities/wallet/model/wallet-resource";
import { VerificationRequired } from "@/features/kyc";
import type { NetworkWithdrawable, Wallet, Withdrawal } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { NetworkMark } from "@/shared/ui/icons/networks";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { Panel, PanelPresence } from "@/shared/ui/motion-panel";
import { formatUsdt, fromBaseUnits, networkLabel, shortAddress, subUsdt, toBaseUnits } from "@/views/wallet/lib/format";
import { NetworkSegments } from "@/views/wallet/ui/network-segments";
import { FieldLabel, WALLET_CARD, WalletScreen } from "@/views/wallet/ui/wallet-chrome";

const FIELD =
  "w-full rounded-lg border border-border bg-ink/20 px-3 py-2.5 text-sm text-ink outline-none placeholder:text-ink-soft focus-visible:ring-2 focus-visible:ring-ring";

// What the user reviewed, frozen at the "Review" click — Confirm submits exactly this
// even if a wallet refetch changes the live selection underneath the open confirm.
interface ReviewedWithdrawal {
  network: string;
  address: string;
  amount: string;
  fee: string | undefined;
  instant: string | undefined;
  rails: string; // the rail list at review time — a changed list voids the review
}

function withdrawableFor(wallet: Wallet | null | undefined, network: string): NetworkWithdrawable | undefined {
  return (wallet?.withdrawable ?? []).find((w) => w.network === network);
}

// Send USDT out to an external address (Figma `cabinet/wallet/withdraw` +
// `cabinet/mobile/wallet/withdraw`). Review is a distinct step: the reviewed figures are
// snapshotted so Confirm can never submit something the user didn't see.
// `onWithdrawn` lets the evinvest.ltd action page (a host's popup) report the request once
// the hub has accepted it; the cabinet's own route leaves it unset.
export function WithdrawView({ initialNetwork, onWithdrawn }: { initialNetwork?: string; onWithdrawn?: (withdrawal: Withdrawal) => void }) {
  const t = useT();
  const locale = useLocale();
  const id = useId();
  const addressId = `${id}-address`;
  const amountId = `${id}-amount`;
  const [selected, setSelected] = useState<string | null>(initialNetwork ?? null);
  const [address, setAddress] = useState("");
  const [amount, setAmount] = useState("");
  const [confirming, setConfirming] = useState<ReviewedWithdrawal | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<unknown>(null);
  const [done, setDone] = useState<Withdrawal | null>(null);
  // Fixed at page load and kept across failed attempts, so a retried POST (a dropped response,
  // a double click) is the same request to the hub and cannot open a second withdrawal.
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());

  const { data: wallet, error: walletError, isLoading: walletLoading } = useResource(walletResource);
  const { data: profile, error: profileError, isLoading: profileLoading } = useResource(profileResource);
  // Below tier 1 the hub refuses the withdrawal itself, so the form could only be filled in
  // and then turned away at Confirm — which is what it did. A failed profile read is not a
  // verdict and leaves the form alone; the submit's own refusal still stops it.
  const gated = (!profileLoading || profileError !== null) && isUnverified(profile);
  // A failed submit is the interesting error here; a failed wallet read only matters while
  // there is no wallet to show, since a stale balance still beats a blank screen.
  const error = submitError ? errorMessage(submitError, t) : wallet || !walletError ? null : errorMessage(walletError, t);

  const networks = (wallet?.withdrawable ?? []).map((w) => w.network ?? "").filter(Boolean);
  const network = (selected && networks.includes(selected) ? selected : null) ?? networks[0] ?? "";

  const opts = withdrawableFor(wallet, network);
  // Exact base-unit math on the USDT strings — no float error on 18-dp amounts.
  const amountUnits = toBaseUnits(amount);
  const youReceive = subUsdt(amount, opts?.withdrawal_fee); // decimal string
  const queuedUnits = amountUnits - toBaseUnits(opts?.instant); // > 0 ⇒ partly queued

  // A wallet refetch that changes the rail list invalidates an open review — the
  // snapshot may point at a rail that no longer exists (guarded render-time reset).
  const rails = networks.join(",");
  if (confirming && confirming.rails !== rails) setConfirming(null);

  const submit = async () => {
    if (submitting || !confirming) return;
    setSubmitting(true);
    setSubmitError(null);
    setDone(null);
    try {
      // The balance, the withdrawal queue and the timeline all move — `submitWithdrawal`
      // names those tags, so this screen's wallet and every other open surface refresh
      // themselves. No explicit reload here any more.
      const withdrawal = await submitWithdrawal({ network: confirming.network, address: confirming.address, amount: confirming.amount, idempotency_key: idempotencyKey });
      setDone(withdrawal);
      // The key names this one request; the next withdrawal from the same page is a new one.
      setIdempotencyKey(crypto.randomUUID());
      onWithdrawn?.(withdrawal);
      setAddress("");
      setAmount("");
    } catch (e) {
      // The error itself, not its English text — `errorMessage` resolves its `code`
      // against the reader's catalogue at the render site.
      setSubmitError(e);
    } finally {
      setSubmitting(false);
      setConfirming(null);
    }
  };

  const valid = network !== "" && address.trim().length > 0 && amountUnits > 0n;
  const label = networkLabel(network);

  return (
    <WalletScreen title={t("ui.withdrawUsdt", "Withdraw USDT")} subtitle={t("wallet.withdrawSub", "Send funds to an external address — one balance, any network")} back="/wallet">
      <StaggerItem>
        <Settled
          loading={walletLoading || profileLoading}
          skeleton={<Skeleton className="h-111 w-full rounded-xl lg:max-w-140" />}
        >
        {walletLoading || profileLoading ? null : gated ? (
        // The balance is not hidden — it is on the wallet screen this one links back to.
        // Only the send is closed, and this says so and offers the one way to open it.
        <VerificationRequired title={t("wallet.withdrawVerifyTitle", "Verify your identity to withdraw")} description={t("wallet.withdrawVerifyBody", "Withdrawals open once your identity is verified. Your balance is untouched in the meantime.")} className="lg:max-w-140" />
      ) : networks.length === 0 ? (
        <p className="text-sm text-ink-soft">{error ?? t("wallet.noWithdrawRails", "No withdrawal networks are available right now — check back soon.")}</p>
      ) : (
        // Form 560 + review 400 side by side is the Figma at 1440; below that the content
        // column can't hold both, so the review wraps under the form rather than overflowing.
        <div className="flex flex-col gap-3.5 lg:flex-row lg:flex-wrap lg:items-start lg:gap-5">
          <div className={cn(WALLET_CARD, "flex flex-col gap-4 p-4.5 lg:w-140 lg:max-w-full lg:flex-none lg:p-6")}>
            <FieldLabel>
              {t("wallet.networkCaps", "NETWORK")}
              <TipAnchor anchor="wallet.withdraw.network" />
            </FieldLabel>
            <NetworkSegments
              networks={networks}
              value={network}
              onChange={(n) => {
                setSelected(n);
                setConfirming(null);
              }}
              label={t("wallet.withdrawalNetwork", "Withdrawal network")}
            />

            <div className="flex flex-col gap-2">
              <FieldLabel htmlFor={addressId}>
                {t("wallet.destinationAddressCaps", "DESTINATION ADDRESS")}
                <TipAnchor anchor="wallet.withdraw.destination" />
              </FieldLabel>
              <input
                id={addressId}
                value={address}
                onChange={(e) => {
                  setAddress(e.target.value);
                  setConfirming(null);
                }}
                placeholder={t("wallet.placeholder.railAddress", "{network} address", { network: label })}
                spellCheck={false}
                className={FIELD}
              />
            </div>

            <div className="flex flex-col gap-2">
              <span className="flex items-center justify-between gap-2">
                <FieldLabel htmlFor={amountId}>{t("wallet.amountCaps", "AMOUNT")}</FieldLabel>
                <span className="flex items-center gap-1.5 text-xs text-ink-soft">
                  {t("wallet.availPrefix", "Avail: {amount}", { amount: formatUsdt(opts?.withdrawable, locale) })}
                  <TipAnchor anchor="wallet.withdraw.available" />
                </span>
              </span>
              {/* The bordered box is the field, not the bare input inside it — so the focus ring
                  belongs on the wrapper, reached from the input via focus-within. */}
              <span className="flex w-full items-center gap-2 rounded-lg border border-border bg-ink/20 py-2 pl-3 pr-2 focus-within:ring-2 focus-within:ring-ring/50">
                <input
                  id={amountId}
                  value={amount}
                  onChange={(e) => {
                    setAmount(e.target.value);
                    setConfirming(null);
                  }}
                  inputMode="decimal"
                  placeholder="0.00"
                  className="min-w-0 flex-1 bg-transparent text-sm text-ink outline-none placeholder:text-ink-soft"
                />
                <button
                  type="button"
                  onClick={() => {
                    setAmount(opts?.withdrawable ?? "0");
                    setConfirming(null);
                  }}
                  className="shrink-0 rounded-md border border-border px-3 py-1.5 text-xs font-medium text-primary-ink outline-none transition-colors hover:bg-ink/5 focus-visible:ring-2 focus-visible:ring-ring"
                >
                  {t("ui.max", "Max")}
                </button>
              </span>
            </div>

            <div className="flex flex-col gap-2.5 rounded-lg bg-secondary px-3.5 py-3">
              <Row label={t("wallet.networkFee", "Network fee")} value={`${formatUsdt(opts?.withdrawal_fee, locale)} USDT`} tip="wallet.withdraw.network-fee" />
              <div className="h-px w-full bg-border" />
              <Row label={t("wallet.youWillReceive", "You will receive")} value={`${formatUsdt(youReceive, locale)} USDT`} tone="text-positive" tip="wallet.withdraw.you-receive" />
            </div>

            {/* `wallet.withdraw.queueing` / `.review` are section-type tips (descriptor blocks,
                not inline ⓘ), so that copy is stated inline here rather than anchored. */}
            {queuedUnits > 0n && amountUnits > 0n && (
              <p className="text-xs text-accent-warn">{t("wallet.exceedsInstant", "~{amount} USDT exceeds instant {network} liquidity and will be queued until the network is topped up.", { amount: formatUsdt(fromBaseUnits(queuedUnits), locale), network: label })}</p>
            )}
            <p className="text-xs text-ink-soft">
              {t("wallet.minInstantQueued", "Min {min} USDT · instant {network} {instant} · rest queued", { min: formatUsdt(opts?.min_withdrawal, locale), network: label, instant: formatUsdt(opts?.instant, locale) })}
            </p>

            <Button
              type="button"
              size="lg"
              disabled={!valid || submitting}
              onClick={() => setConfirming({ network, address, amount, fee: opts?.withdrawal_fee, instant: opts?.instant, rails })}
              className="w-full font-semibold"
            >
              {t("wallet.reviewWithdrawal", "Review withdrawal")}
            </Button>
          </div>

          <div className="flex flex-col gap-3.5 empty:hidden lg:w-100 lg:max-w-full lg:flex-none lg:gap-5">
            {/* Receipt, failure and the review step all land in this column and replace
                one another. One presence boundary over the three means a submit that
                fails swaps review → error in place, instead of the column emptying and
                refilling under the reader's eye. */}
            <PanelPresence>
              {done && (
                <Panel key="receipt" from="bottom" className={cn(WALLET_CARD, "flex gap-3 p-4.5 lg:p-5")}>
                  <Clock className="mt-0.5 size-4 shrink-0 text-accent-warn" />
                  <div className="min-w-0">
                    <p className="text-sm font-semibold text-ink">{(done.state === "queued" ? t("wallet.withdrawalQueued", "Withdrawal queued") : t("wallet.withdrawalSubmitted", "Withdrawal submitted"))}</p>
                    <p className="text-xs text-ink-soft">
                      {done.state === "queued"
                        ? t("wallet.receiptQueued", "{amount} USDT to {address} is queued — it is sent once {network} liquidity is topped up.", { amount: formatUsdt(done.net_amount, locale), address: shortAddress(done.address), network: networkLabel(done.network) })
                        : t("wallet.receiptSubmitted", "{amount} USDT is on its way to {address} — pending on-chain confirmation.", { amount: formatUsdt(done.net_amount, locale), address: shortAddress(done.address) })}
                    </p>
                  </div>
                </Panel>
              )}

              {error && (
                <Panel key="error" from="bottom" className={cn(WALLET_CARD, "flex gap-3 border-accent-error/50 p-4.5 lg:p-5")}>
                  <TriangleAlert className="mt-0.5 size-4 shrink-0 text-accent-error" />
                  <div className="min-w-0">
                    <p className="text-sm font-semibold text-ink">{t("wallet.withdrawalFailed", "Withdrawal failed")}</p>
                    <p className="text-xs text-ink-soft">{error}</p>
                  </div>
                </Panel>
              )}

              {confirming && (
                <Panel key="review" from="bottom" className={cn(WALLET_CARD, "flex flex-col gap-4 p-4.5 lg:p-5")}>
                  <p className="text-sm font-semibold text-ink">{t("wallet.reviewWithdrawal", "Review withdrawal")}</p>
                  <div className="flex flex-col gap-2.5">
                    <Row
                      label={t("ui.network", "Network")}
                      value={
                        <span className="inline-flex items-center gap-1.5">
                          <NetworkMark network={confirming.network} className="size-3.5 shrink-0" />
                          {networkLabel(confirming.network)}
                        </span>
                      }
                    />
                    <Row label={t("ui.destination", "Destination")} value={shortAddress(confirming.address)} />
                    <Row label={t("ui.amount", "Amount")} value={`${formatUsdt(confirming.amount, locale)} USDT`} />
                    <Row label={t("wallet.networkFee", "Network fee")} value={`${formatUsdt(confirming.fee, locale)} USDT`} />
                    <div className="h-px w-full bg-border" />
                    <Row label={t("wallet.youWillReceive", "You will receive")} value={`${formatUsdt(subUsdt(confirming.amount, confirming.fee), locale)} USDT`} tone="text-positive" />
                  </div>
                  <p className="break-all font-mono-tech text-xs text-ink-soft">{t("wallet.toAddressLine", "To {address}", { address: confirming.address })}</p>
                  {toBaseUnits(confirming.amount) - toBaseUnits(confirming.instant) > 0n && (
                    <p className="text-xs text-accent-warn">
                      {t("wallet.exceedsInstant", "~{amount} USDT exceeds instant {network} liquidity and will be queued until the network is topped up.", {
                        amount: formatUsdt(fromBaseUnits(toBaseUnits(confirming.amount) - toBaseUnits(confirming.instant)), locale),
                        network: networkLabel(confirming.network),
                      })}
                    </p>
                  )}
                  {/* A `flex-1` Confirm beside a fixed-width Back in a ≤400px column.
                      i18n-max: 20 on the confirm label, 11 on the back one. */}
                  <div className="flex gap-2">
                    <Button type="button" size="lg" disabled={submitting} onClick={submit} className="min-w-0 flex-1 font-semibold">
                      {submitting && <Spinner aria-hidden />}
                      <span className="truncate">{t("wallet.confirmWithdrawal", "Confirm withdrawal")}</span>
                    </Button>
                    <Button type="button" size="lg" variant="outline" disabled={submitting} onClick={() => setConfirming(null)} className="shrink-0">
                      {t("ui.back", "Back")}
                    </Button>
                  </div>
                  <p className="text-xs text-ink-soft">{t("wallet.acceptedInstantly", "Accepted; if {network} liquidity is short right now, it queues until topped up — you'll be notified.", { network: networkLabel(confirming.network) })}</p>
                </Panel>
              )}
            </PanelPresence>
          </div>
        </div>
        )}
        </Settled>
      </StaggerItem>
    </WalletScreen>
  );
}

// `value` takes a node, not a string: the network row pairs the chain's mark with its
// name, while every other row is a figure.
function Row({ label, value, tone, tip }: { label: string; value: ReactNode; tone?: string; tip?: TipKey }) {
  return (
    <div className="flex items-center justify-between gap-3">
      <span className="flex items-center gap-1.5 text-xs text-ink-soft">
        {label}
        {tip && <TipAnchor anchor={tip} />}
      </span>
      <span className={cn("text-sm font-medium tabular-nums", tone ?? "text-ink")}>{value}</span>
    </div>
  );
}
