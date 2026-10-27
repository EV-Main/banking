"use client";

import { useLocale, useT } from "@evinvest/i18n/react";

import { useId, useState } from "react";

import { Button, Skeleton, Spinner } from "@evinvest/uikit";

import { isUnverified } from "@/entities/user/lib/kyc";
import { profileResource } from "@/entities/user/model/profile-resource";
import { walletResource } from "@/entities/wallet/model/wallet-resource";
import { VerificationRequired } from "@/features/kyc";
import { ConfirmStep, firstPartyIntentClient, IntentErrorAlert, StepupSurface, useIntentFlow } from "@/features/wallet-intent";
import type { NetworkWithdrawable, Wallet, Withdrawal } from "@/shared/contracts";
import { errorMessage } from "@/shared/lib/api-client";
import { TAG } from "@/shared/lib/cache-tags";
import { cn } from "@/shared/lib/cn";
import { revalidateTag, useResource } from "@/shared/lib/resource";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { Panel, PanelPresence } from "@/shared/ui/motion-panel";
import { formatUsdt, fromBaseUnits, networkLabel, subUsdt, toBaseUnits } from "@/views/wallet/lib/format";
import { NetworkSegments } from "@/views/wallet/ui/network-segments";
import { FieldLabel, WALLET_CARD, WalletScreen } from "@/views/wallet/ui/wallet-chrome";

const FIELD =
  "w-full rounded-lg border border-border bg-ink/20 px-3 py-2.5 text-sm text-ink outline-none placeholder:text-ink-soft focus-visible:ring-2 focus-visible:ring-ring";

function withdrawableFor(wallet: Wallet | null | undefined, network: string): NetworkWithdrawable | undefined {
  return (wallet?.withdrawable ?? []).find((w) => w.network === network);
}

// Send USDT out to an external address (Figma `cabinet/wallet/withdraw` +
// `cabinet/mobile/wallet/withdraw`). Review shows the BFF's terms for the typed inputs, and the
// form stays locked until the person goes back, so Confirm can only send what is on screen.
// `onWithdrawn` reports the request once the hub has accepted it.
export function WithdrawView({ initialNetwork, onWithdrawn }: { initialNetwork?: string; onWithdrawn: (withdrawal: Withdrawal) => void }) {
  const t = useT();
  const locale = useLocale();
  const id = useId();
  const addressId = `${id}-address`;
  const amountId = `${id}-amount`;
  const [selected, setSelected] = useState<string | null>(initialNetwork ?? null);
  const [address, setAddress] = useState("");
  const [amount, setAmount] = useState("");
  const controls = useIntentFlow<"withdraw">(firstPartyIntentClient, ({ result }) => {
    revalidateTag(TAG.wallet, TAG.withdrawals, TAG.operations);
    onWithdrawn(result);
  });
  const { flow } = controls;
  const locked = flow.phase !== "editing";

  const { data: wallet, error: walletError, isLoading: walletLoading } = useResource(walletResource);
  const { data: profile, error: profileError, isLoading: profileLoading } = useResource(profileResource);
  // Below tier 1 the hub refuses the withdrawal itself, so the form could only be filled in
  // and then turned away at Confirm — which is what it did. A failed profile read is not a
  // verdict and leaves the form alone; the submit's own refusal still stops it.
  const gated = (!profileLoading || profileError !== null) && isUnverified(profile);
  // A failed wallet read only matters while there is no wallet to show, since a stale balance
  // still beats a blank screen.
  const error = wallet || !walletError ? null : errorMessage(walletError, t);

  const networks = (wallet?.withdrawable ?? []).map((w) => w.network ?? "").filter(Boolean);
  const network = (selected && networks.includes(selected) ? selected : null) ?? networks[0] ?? "";

  const opts = withdrawableFor(wallet, network);
  // Exact base-unit math on the USDT strings — no float error on 18-dp amounts.
  const amountUnits = toBaseUnits(amount);
  const youReceive = subUsdt(amount, opts?.withdrawal_fee); // decimal string
  const queuedUnits = amountUnits - toBaseUnits(opts?.instant); // > 0 ⇒ partly queued


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
          {/* Locked while the BFF holds terms for these inputs; Back on the review unlocks it. */}
          <fieldset disabled={locked} className={cn(WALLET_CARD, "m-0 flex min-w-0 flex-col gap-4 p-4.5 lg:w-140 lg:max-w-full lg:flex-none lg:p-6")}>
            <FieldLabel>
              {t("wallet.networkCaps", "NETWORK")}
              <TipAnchor anchor="wallet.withdraw.network" />
            </FieldLabel>
            <NetworkSegments
              networks={networks}
              value={network}
              onChange={setSelected}
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
                onChange={(e) => setAddress(e.target.value)}
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
                  onChange={(e) => setAmount(e.target.value)}
                  inputMode="decimal"
                  placeholder="0.00"
                  className="min-w-0 flex-1 bg-transparent text-sm text-ink outline-none placeholder:text-ink-soft"
                />
                <button
                  type="button"
                  onClick={() => setAmount(opts?.withdrawable ?? "0")}
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
              disabled={!valid}
              aria-busy={flow.phase === "creating"}
              onClick={() => controls.submit({ kind: "withdraw", params: { network, address: address.trim(), amount: amount.trim() } })}
              className="w-full font-semibold"
            >
              {flow.phase === "creating" && <Spinner aria-hidden />}
              {t("wallet.reviewWithdrawal", "Review withdrawal")}
            </Button>
          </fieldset>

          <div className="flex flex-col gap-3.5 empty:hidden lg:w-100 lg:max-w-full lg:flex-none lg:gap-5">
            {/* A refused create and the review land in this column and replace one another. One
                presence boundary over both swaps them in place, instead of the column emptying
                and refilling under the reader's eye. */}
            <PanelPresence>
              {flow.phase === "editing" && flow.error && (
                <Panel key="error" from="bottom">
                  <IntentErrorAlert error={flow.error} />
                </Panel>
              )}

              {(flow.phase === "review" || flow.phase === "expired" || flow.phase === "unclear") && (
                <Panel key="review" from="bottom" className={cn(WALLET_CARD, "flex flex-col gap-4 p-4.5 lg:p-5")}>
                  <p className="text-sm font-semibold text-ink">{t("wallet.reviewWithdrawal", "Review withdrawal")}</p>
                  <ConfirmStep kind="withdraw" controls={controls} />
                  <StepupSurface dialog={controls.stepupDialog} scopeHost={null} />
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

function Row({ label, value, tone, tip }: { label: string; value: string; tone?: string; tip?: TipKey }) {
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
