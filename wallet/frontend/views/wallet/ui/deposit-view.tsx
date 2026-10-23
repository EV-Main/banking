"use client";

import type { Locale } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";

import { Check, Copy, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";

import { Button, Skeleton } from "@evinvest/uikit";

import { isUnverified } from "@/entities/user/lib/kyc";
import { profileResource } from "@/entities/user/model/profile-resource";
import { depositAddressResource, walletResource } from "@/entities/wallet/model/wallet-resource";
import { CustodyNote, VerificationRequired } from "@/features/kyc";
import { errorMessage, isVerificationRequired } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { Settled, StaggerItem } from "@/shared/ui/motion";
import { displayAddress } from "@/shared/lib/ton-address";
import { TipAnchor } from "@/shared/tips";
import { NetworkMark } from "@/shared/ui/icons/networks";
import { isEvmRail, networkLabel } from "@/views/wallet/lib/format";
import { DepositQr, preloadDepositQr } from "@/views/wallet/ui/deposit-qr";
import { NetworkSegments } from "@/views/wallet/ui/network-segments";
import { useFirstDepositSignal } from "@/views/wallet/model/use-first-deposit-signal";
import { FieldLabel, WALLET_CARD, WalletScreen } from "@/views/wallet/ui/wallet-chrome";

// Top up the balance with crypto (Figma `cabinet/wallet/deposit` + `cabinet/mobile/wallet/deposit`):
// pick a rail, then copy the address it maps to. The address is fetched per rail from the BFF —
// the rails on offer come from the wallet response, so an unwatched network never appears.
// "BEP20 or Polygon" — the conjunction used to be hard-coded as `.join(" or ")`, which is
// the English one. `Intl.ListFormat` is the locale's own, and needs no catalogue entry.
function orList(items: string[], locale: Locale): string {
  return new Intl.ListFormat(locale, { style: "long", type: "disjunction" }).format(items);
}

export function DepositView({ initialNetwork }: { initialNetwork?: string }) {
  const t = useT();
  useFirstDepositSignal();
  const locale = useLocale();
  const [selected, setSelected] = useState<string | null>(initialNetwork ?? null);
  const [copied, setCopied] = useState(false);
  // Overlaps the QR encoder's download with the address request instead of following it.
  useEffect(preloadDepositQr, []);

  const { data: wallet, error: walletError, isLoading: walletLoading } = useResource(walletResource);
  // Shared with the home card's status chip, so the tier is often known on the first
  // frame of this screen.
  const { data: profile, error: profileError, isLoading: profileLoading } = useResource(profileResource);

  // The tier is unknown only while the read is in flight. A read that FAILED must not gate
  // anything — a verified user would lose their address to an unrelated blip — so it falls
  // through to the request, whose own refusal still names the gate.
  const tierKnown = !profileLoading || profileError !== null;
  const gated = tierKnown && isUnverified(profile);

  const networks = (wallet?.deposit_addresses ?? []).map((a) => a.network ?? "").filter(Boolean);
  // A `?network=` that no longer maps to a live rail falls back to the first one on offer.
  const network = (selected && networks.includes(selected) ? selected : null) ?? networks[0] ?? "";

  // Keyed on the rail, so switching back to a network already looked at re-shows its
  // address and QR immediately instead of re-fetching an address that cannot have changed.
  // An empty rail disables the read (`enabled` on the resource): below tier 1 the hub has no
  // address to give on any network, so the request is not made rather than made and refused.
  const { data: address, error: addressError, isLoading: addressLoading } = useResource(depositAddressResource, tierKnown && !gated ? network : "");

  // The gate, whether the profile named it or the hub did. The second path covers a tier
  // that moved in another tab, or a profile too stale to have caught up.
  const needsVerification = gated || isVerificationRequired(addressError);

  const error = (wallet || !walletError ? null : errorMessage(walletError, t)) ?? (address || !addressError ? null : errorMessage(addressError, t)) ?? null;

  // The address swap is now a consequence of the key changing; only the copy affordance
  // carries state that has to be reset by hand.
  const selectNetwork = (next: string) => {
    setSelected(next);
    setCopied(false);
  };

  // TON's raw `workchain:hex` form is valid but wallet-hostile — show the friendly
  // non-bounceable UQ… form (an uninitialized deposit wallet bounces EQ… sends).
  const shown = displayAddress(network, address?.address ?? "", { testnet: address?.is_testnet });

  const copy = () => {
    if (!shown) return;
    void navigator.clipboard.writeText(shown);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  // Other EVM rails on offer: their `0x` deposit addresses look identical to this one, so a
  // BEP20/Polygon mix-up sends to a valid-but-unwatched address. Name them explicitly.
  const evmSiblings = isEvmRail(network) ? networks.filter((n) => n !== network && isEvmRail(n)).map(networkLabel) : [];
  const label = networkLabel(network);

  return (
    <WalletScreen title={t("ui.depositUsdt", "Deposit USDT")} subtitle={t("ui.receiveFunds", "Receive funds into your wallet")} back="/wallet">
      {/* One section, not two: the network picker and the address card are handed over
          together by the same `Settled`, so they arrive together too. Staggering them
          would need the stagger to start after the read landed, which is a second
          entrance stacked on the first — exactly the seam the slice exists to avoid. */}
      <StaggerItem className="flex w-full flex-col gap-3.5 lg:max-w-160 lg:gap-5">
        {/* The wrapper repeats the column so the two children keep their gap:
            `display: contents` would preserve the parent's layout but makes
            opacity and transform no-ops, which is the whole point here. */}
        <Settled
          className="flex flex-col gap-3.5 lg:gap-5"
          loading={walletLoading || profileLoading}
          skeleton={
            <>
              <Skeleton className="h-26 rounded-xl" />
              <Skeleton className="h-90 rounded-xl" />
            </>
          }
        >
          {walletLoading || profileLoading ? null : needsVerification ? (
            // No rail picker and no network warning beside it: there is no address on any
            // network yet, so a selector would offer a choice that changes nothing, and a
            // warning about sending to the wrong chain has nothing to be sent to.
            <VerificationRequired title={t("wallet.depositVerifyTitle", "Verify your identity to deposit")} description={t("wallet.depositVerifyBody", "Your deposit address opens once your identity is verified. It only needs doing once.")} />
          ) : networks.length === 0 ? (
            <p className="text-sm text-ink-soft">
              {error ?? t("wallet.noDepositRails", "No deposit networks are available right now — check back soon.")}
            </p>
          ) : (
            <>
              <div className={cn(WALLET_CARD, "flex flex-col gap-3.5 p-4.5 lg:p-6")}>
                <FieldLabel>
                  {t("wallet.selectNetwork", "SELECT NETWORK")}
                  <TipAnchor anchor="wallet.deposit.network" />
                </FieldLabel>
                <NetworkSegments networks={networks} value={network} onChange={selectNetwork} label={t("wallet.depositNetwork", "Deposit network")} />
              </div>

              <div className={cn(WALLET_CARD, "flex flex-col items-center gap-4 p-4.5 lg:gap-4.5 lg:p-6")}>
                <p className="flex items-center gap-1.5 text-sm font-semibold text-ink">
                  {/* The mark leads the line rather than being spliced into it: the
                      sentence is one translated string, and cutting a component into the
                      middle of it would fix the chain's position in word order for every
                      locale. */}
                  <NetworkMark network={network} className="size-4 shrink-0" />
                  {t("wallet.yourDepositAddress", "Your {network} deposit address", { network: label })}
                  <TipAnchor anchor="wallet.deposit.address" />
                </p>

                {error && !shown && <p className="text-sm text-accent-error">{error}</p>}

                {addressLoading ? (
                  <>
                    <Skeleton className="size-40 rounded-xl lg:size-45 lg:rounded-2xl" />
                    <Skeleton className="h-10 w-full rounded-lg" />
                  </>
                ) : address && shown ? (
                  <>
                    <DepositQr value={shown} />
                    <div className="flex w-full items-center justify-between gap-2 rounded-lg border border-border bg-ink/20 px-3 py-2.5 lg:py-2.5 lg:pl-3.5 lg:pr-2">
                      <code className="min-w-0 flex-1 break-all font-sans text-xs text-ink lg:text-sm">{shown}</code>
                      {/* i18n-max: 11 — a `shrink-0` button beside the address it squeezes. */}
                      <Button type="button" size="sm" onClick={copy} className="hidden shrink-0 lg:inline-flex">
                        {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
                        {copied ? t("ui.copied", "Copied") : t("ui.copy", "Copy")}
                      </Button>
                    </div>
                    <Button type="button" onClick={copy} className="w-full lg:hidden">
                      {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
                      {copied ? t("ui.copied", "Copied") : t("wallet.copyAddress", "Copy address")}
                    </Button>
                    <p className="flex items-center justify-center gap-1.5 text-center text-xs text-ink-soft">
                      {t("wallet.creditedAfterConfirmations", "Credited to your one balance after {n, plural, one {# network confirmation} other {# network confirmations}}.", { n: address.min_confirmations ?? 0 })}
                      <TipAnchor anchor="wallet.deposit.min-confirmations" />
                    </p>
                  </>
                ) : (
                  <p className="text-center text-sm text-ink-soft">{t("wallet.depositAddressUnavailable", "A {network} deposit address isn't available yet — this network is still being set up. Check back soon.", { network: label })}</p>
                )}
              </div>

              <CustodyNote className="px-1" />

              <div className="flex gap-2.5 rounded-xl border border-accent-warn bg-accent-warn/5 px-4 py-3.5 lg:gap-3 lg:px-4.5 lg:py-4">
                <TriangleAlert className="mt-px size-4 shrink-0 text-accent-warn" />
                <div className="flex min-w-0 flex-col gap-1">
                  {/* `wallet.deposit.rail-hazard` is a section-type tip (a descriptor block, not an
                      inline ⓘ) — this card already carries that copy, so it isn't anchored here. */}
                  <p className="text-sm font-semibold text-ink">{t("wallet.networkWarning", "Network warning")}</p>
                  {/* One sentence per key. The `not` used to be a `<strong>` and the `0x` a
                      `<code>`, which meant the warning was three fragments a translator
                      could not reorder — and German and Russian both put the negation
                      somewhere else in this clause. The block already carries its own
                      alarm (amber border, warning glyph, bold heading), so the sentence is
                      whole and the emphasis lives on the card rather than inside the words. */}
                  <p className="text-xs text-ink-soft">
                    {t("wallet.railHazard", "Only send USDT on the {network} network to this address. Funds sent on any other network will be lost permanently.", { network: label })}
                    {evmSiblings.length > 0 && ` ${t("wallet.evmSiblingHazard", "This {network} address is not your {siblings} address — even though both start with 0x. USDT sent on {siblingsOr} will not be credited.", { network: label, siblings: evmSiblings.join(" / "), siblingsOr: orList(evmSiblings, locale) })}`}
                  </p>
                </div>
              </div>
            </>
          )}
        </Settled>
      </StaggerItem>
    </WalletScreen>
  );
}
