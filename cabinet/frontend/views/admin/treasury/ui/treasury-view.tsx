"use client";

import { Check, Copy, Loader2, RefreshCw, TriangleAlert } from "lucide-react";
import { type ReactNode, useCallback, useState } from "react";

import type { Locale, Translate } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";
import { Button, Card, CardContent, Input, Select, SelectContent, SelectItem, SelectTrigger, Skeleton } from "@evinvest/uikit";

import { recordTreasuryDeposit, type RecordedArrival } from "@/entities/admin/api/admin-client";
import { treasuryResource } from "@/entities/admin/model/admin-resource";
import type { RailLiquidity } from "@/shared/contracts/admin";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { RichMessage } from "@/shared/ui/rich-message";
import { useResource } from "@/shared/lib/resource";
import { displayAddress } from "@/shared/lib/ton-address";
import { TipAnchor, type TipKey } from "@/shared/tips";
import { NetworkMark } from "@/shared/ui/icons/networks";
import { formatUsd, railLabel } from "@/views/admin/lib/format";
import { StaggerItem } from "@/shared/ui/motion";
import { ResourceError } from "@/shared/ui/resource-error";
import { AdminHeader, AdminScreen } from "@/views/admin/ui/shell";

const TEAL_CTA = "bg-accent-debug text-background hover:bg-accent-debug/90";

const GAS_SYMBOLS: Record<string, string> = {
  bep20: "BNB",
  trc20: "TRX",
  ton: "TON",
  polygon: "POL",
};

export function TreasuryView() {
  const t = useT();
  // Cached, so returning from another console screen paints the figures immediately and
  // refreshes them behind. A failed read must STOP the skeletons — pulsing placeholders
  // beside an error read as "still coming", so the retry never gets clicked — which is
  // exactly what `isLoading` reports: false once an attempt has settled either way.
  const read = useResource(treasuryResource);
  const treasury = read.data ?? null;
  const error = read.error ? errorMessage(read.error, t) : null;
  const loading = read.isLoading || read.isValidating;
  const retry = () => void read.refresh();

  return (
    <AdminScreen className="space-y-8">
      <AdminHeader
        eyebrow={t("admin.eyebrow.administer", "Administer")}
        title={t("nav.treasury", "Treasury")}
        subtitle={t("admin.treasury.subtitle", "Two layers — user claims (USDT) vs on-chain liquidity by rail")}
        action={
          <Button type="button" variant="outline" size="sm" disabled={loading} onClick={retry}>
            <RefreshCw className={loading ? "size-4 animate-spin" : "size-4"} /> {t("ui.refresh", "Refresh")}
          </Button>
        }
      />

      {error && <ResourceError message={error} onRetry={retry} retrying={loading} />}

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.treasury.layer1", "Layer 1 · Ledger — user claims (USDT)")}</p>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <MoneyCard label={t("admin.treasury.claimsTotal", "Claims · total (USDT)")} value={treasury?.total_custody} hint={t("admin.treasury.claimsTotalHint", "= on-chain custody · backed")} loading={loading && !treasury} unavailable={!loading && !treasury} tip="admin.treasury.layer1.claims-total" />
          <MoneyCard label={t("admin.treasury.heldForClients", "Held for clients")} value={treasury?.held_for_clients} hint={t("admin.treasury.heldForClientsHint", "user + service balances")} loading={loading && !treasury} unavailable={!loading && !treasury} tip="admin.treasury.layer1.held-for-clients" />
          <MoneyCard label={t("admin.treasury.fundCapital", "Fund capital")} value={treasury?.fund_capital} hint={t("admin.treasury.fundCapitalHint", "company's own")} loading={loading && !treasury} unavailable={!loading && !treasury} tip="admin.treasury.layer1.fund-capital" />
          {/* The fee claim was load-bearing but invisible here: `held_for_clients` is
              derived as total − fund capital − THIS, so without it the figures above
              don't add up. It is also exactly what the Fund revenue screen pays out. */}
          <MoneyCard label={t("nav.revenue", "Fund revenue")} value={treasury?.fee_revenue} hint={t("admin.treasury.feeRevenueHint", "earned — fees + settled 2-and-20")} loading={loading && !treasury} unavailable={!loading && !treasury} />
          <MoneyCard label={t("admin.treasury.reservedWithdrawals", "Reserved · withdrawals")} value={treasury?.reserved_for_withdrawals} hint={t("admin.treasury.reservedWithdrawalsHint", "queued + in-flight (clearing)")} loading={loading && !treasury} unavailable={!loading && !treasury} tip="admin.treasury.layer1.reserved-withdrawals" />
        </div>
      </StaggerItem>

      <StaggerItem as="section" className="space-y-3">
        <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.treasury.layer2", "Layer 2 · Treasury — liquidity by rail")}</p>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {treasury ? (
            <>
              {treasury.rails.map((rail) => (
                <MoneyCard key={rail.network} network={rail.network} label={railLabel(rail.network, t)} value={rail.custody} loading={false} footer={<RailFunding rail={rail} />} />
              ))}
              <MoneyCard label={t("admin.treasury.bank", "Bank · USD")} value={treasury.bank} hint={t("admin.treasury.bankHint", "off-ramp · FX")} loading={false} tip="admin.treasury.bank" />
            </>
          ) : (
            Array.from({ length: 4 }).map((_, i) => <MoneyCard key={i} label="" value={undefined} loading={loading} unavailable={!loading} />)
          )}
        </div>
      </StaggerItem>

      <RecordArrival rails={treasury?.rails} onRecorded={retry} />

      {/* The expression is code, so it is an ICU argument rather than a key of its own —
          and the sentence around it stays whole, which is what a translator needs to put
          it where their grammar wants it. `RichMessage` is what lets that argument render
          as `<code>` rather than as prose: an invariant set in the body face reads as
          something someone wrote, not as something the system enforces. */}
      <StaggerItem as="p" className="max-w-3xl text-xs text-ink-soft">
        <RichMessage
          message={(t, v) => t("admin.treasury.invariantNote", "Per-rail backing is the treasury's job, not the ledger's: a shortfall on one rail is accept-and-queue, then rebalanced via CEX / alt-rail / top-up. The global invariant is {invariant}.", v)}
          values={{ invariant: <code className="font-mono-tech">sum(custody) == sum(claims)</code> }}
        />
      </StaggerItem>
    </AdminScreen>
  );
}

/** Funding a treasury hot wallet directly moves real USDT while writing nothing to the
 *  ledger: the rail's custody figure doesn't move, fund capital understates what went in,
 *  and the dispatch gate (`min(TB rail, on-chain treasury)`) keeps reading the old number,
 *  so that liquidity can't be withdrawn. This is where that arrival gets recorded.
 *
 *  Idempotent by `tx_ref`, so the honest outcome is three-way: credited, already credited,
 *  or failed — collapsing "already credited" into a generic success would invite the
 *  operator to re-submit under a second reference and double-count the same dollar. */
function RecordArrival({ rails, onRecorded }: { rails: RailLiquidity[] | undefined; onRecorded: () => void }) {
  const t = useT();
  const [network, setNetwork] = useState("");
  const [txRef, setTxRef] = useState("");
  const [amount, setAmount] = useState("");
  const [state, setState] = useState<{ busy: boolean; error: string | null; result: RecordedArrival | null }>({ busy: false, error: null, result: null });

  // Only rails the hub actually reported: an address minted for a rail nothing watches is
  // exactly the mistake this screen exists to prevent.
  const options = rails?.filter((r) => r.treasury_address) ?? [];

  const submit = useCallback(() => {
    setState({ busy: true, error: null, result: null });
    // The amount goes as an ASSERTION, and only when the operator typed one — the hub reads
    // the real figure off the chain. Sending it as a value is what would let this mint money.
    recordTreasuryDeposit({ tx_ref: txRef.trim(), network, expected_amount: amount.trim() || undefined })
      .then((res) => {
        setState({ busy: false, error: null, result: res });
        if (res.recorded) {
          setTxRef("");
          setAmount("");
          onRecorded();
        }
      })
      .catch((e: Error) => setState({ busy: false, error: errorMessage(e, t), result: null }));
  }, [txRef, network, amount, onRecorded, t]);

  return (
    <StaggerItem as="section" className="space-y-3">
      <p className="text-xs font-semibold uppercase tracking-widest text-ink-soft">{t("admin.treasury.recordArrival", "Record an out-of-band arrival")}</p>
      <Card>
        <CardContent className="space-y-5 py-6">
          <p className="max-w-3xl text-sm text-ink-soft">{t("admin.treasury.recordArrivalIntro", "USDT sent straight to a treasury hot wallet is real money the ledger never saw. Give its on-chain reference and the hub reads the transfer back off the chain — the amount and the party credited come from there, so this records an arrival rather than asserting one.")}</p>
          <div className="grid gap-4 md:grid-cols-3">
            <div className="flex flex-col gap-1.5">
              <span className="text-sm text-ink-soft">{t("admin.rail", "Rail")}</span>
              <Select value={network} onValueChange={setNetwork}>
                <SelectTrigger className="w-full border-border bg-secondary" disabled={options.length === 0}>
                  {/* The placeholder is trigger text, not a selectable item — "Select a
                      rail…" is not a rail. */}
                  <span className={cn("flex min-w-0 items-center gap-1.5", !network && "text-ink-soft")}>
                    {network && <NetworkMark network={network} className="size-3.5 shrink-0" />}
                    <span className="truncate">
                      {network ? railLabel(network, t) : options.length === 0 ? t("admin.treasury.noRailWithTreasury", "No rail with a treasury") : t("admin.treasury.selectRail", "Select a rail…")}
                    </span>
                  </span>
                </SelectTrigger>
                <SelectContent>
                  {options.map((r) => (
                    <SelectItem key={r.network} value={r.network}>
                      <NetworkMark network={r.network} className="size-3.5 shrink-0" />
                      {railLabel(r.network, t)}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <label className="flex flex-col gap-1.5">
              <span className="text-sm text-ink-soft">{t("admin.treasury.expectedAmount", "Expected amount (USDT) · optional")}</span>
              <Input value={amount} onChange={(e) => setAmount(e.target.value)} inputMode="decimal" placeholder={t("admin.treasury.placeholder.any", "any")} className="w-full" />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-sm text-ink-soft">{t("admin.treasury.onchainRef", "On-chain reference")}</span>
              {/* A format literal, not prose — it reads the same in every locale. */}
              <Input value={txRef} onChange={(e) => setTxRef(e.target.value)} placeholder="0xhash:logIndex" className="w-full" />
            </label>
          </div>

          {/* The two reference formats are code, so they ride in as ICU arguments and the
              note stays one key: a translator needs the whole sentence to place them, and
              splitting around the two spans would hand them three fragments instead. */}
          <p className="text-xs text-ink-soft">
            <RichMessage
              message={(t, v) => t("admin.treasury.refNote", "The reference is both what gets verified and the idempotency key — {evmRef} on an EVM rail, {tonRef} on TON. A reference that names no confirmed transfer to one of our addresses is refused, and a re-submission of one already recorded is a no-op. Fill the amount only to assert what you expect; a mismatch is then an error instead of a surprise.", v)}
              values={{
                evmRef: <code className="font-mono-tech">txhash:logIndex</code>,
                tonRef: <code className="font-mono-tech">txhash:piggybank</code>,
              }}
            />
          </p>

          {state.error && (
            <p className="flex items-center gap-2 text-sm text-accent-error">
              <TriangleAlert className="size-4 shrink-0" /> {state.error}
            </p>
          )}
          {state.result?.recorded && (
            <p className="text-sm text-accent-debug">
              {t("admin.treasury.recorded", "Recorded — the chain reported {amount}, credited to {party}.", { amount: formatUsd(state.result.amount), party: partyLabel(state.result, t) })}
            </p>
          )}
          {state.result && !state.result.recorded && <p className="text-sm text-accent-warn">{t("admin.treasury.alreadyRecorded", "Already recorded — that reference was credited before, nothing changed.")}</p>}

          <Button type="button" className={cn("ml-auto flex", TEAL_CTA)} disabled={state.busy || !network || !txRef.trim()} onClick={submit}>
            {state.busy ? <Loader2 className="size-4 animate-spin" /> : null}
            {t("admin.treasury.recordArrivalSubmit", "Record arrival")}
          </Button>
        </CardContent>
      </Card>
    </StaggerItem>
  );
}

/** Who the chain said the money belongs to. Worth showing rather than assuming the fund:
 *  a reference that turns out to be a user's deposit credits that user, and the operator
 *  should see that happened instead of reading it as company capital. */
function partyLabel({ party_kind, party_id }: RecordedArrival, t: Translate): string {
  if (party_kind === "piggybank") return t("admin.treasury.party.fundCapital", "fund capital");
  return party_id ? t("admin.treasury.party.generic", "{kind} {id}", { kind: party_kind, id: party_id }) : party_kind;
}

/** `unavailable` is the read-failed state: a muted dash, never a formatted `$0.00` —
 *  a zero the treasury never reported would be read as a real balance. */
// `network` is set only on the per-rail cards; the fund-level ones (bank, reserved) name
// no chain and get no mark.
function MoneyCard({ label, network, value, hint, loading, unavailable, footer, tip }: { label: string; network?: string; value: string | undefined; hint?: string; loading: boolean; unavailable?: boolean; footer?: ReactNode; tip?: TipKey }) {
  return (
    <Card>
      <CardContent className="space-y-1 py-5">
        <div className="flex items-center gap-1.5">
          {network && <NetworkMark network={network} className="size-3.5 shrink-0 text-ink-soft" />}
          <p className="text-xs text-ink-soft">{label || "…"}</p>
          {tip && <TipAnchor anchor={tip} />}
        </div>
        {loading ? (
          <Skeleton className="mt-1 h-8 w-28" />
        ) : unavailable ? (
          <p className="text-3xl font-semibold tabular-nums text-ink-soft">—</p>
        ) : (
          <p className="text-3xl font-semibold tabular-nums">{formatUsd(value)}</p>
        )}
        {hint && !loading && !unavailable && <p className="text-xs text-positive">{hint}</p>}
        {footer && !loading && footer}
      </CardContent>
    </Card>
  );
}

/** The rail's hot-wallet funding picture — address + on-chain USDT/gas, "—" when the
 * treasury read was unavailable (the hub degrades to empty, never fails). */
function RailFunding({ rail }: { rail: RailLiquidity }) {
  const t = useT();
  const locale = useLocale();
  const gasSymbol = GAS_SYMBOLS[rail.network] ?? "";
  // The hub stores TON addresses raw (`workchain:hex`) — an operator can't recognise or
  // paste that into a wallet, so render the same friendly form the deposit screen shows.
  const show = (address: string) => displayAddress(rail.network, address, { testnet: rail.is_testnet });

  return (
    <div className="space-y-2 border-t border-border pt-2.5">
      {rail.treasury_address ? (
        <div className="space-y-1">
          <div className="flex items-center gap-1.5">
            <p className="text-xs text-ink-soft">{t("nav.treasury", "Treasury")}</p>
            <TipAnchor anchor="admin.treasury.rail.address" />
          </div>
          <CopyableAddress address={show(rail.treasury_address)} />
        </div>
      ) : (
        <p className="text-xs text-ink-soft">{t("admin.treasury.custodyUnconfigured", "— · custody unconfigured")}</p>
      )}
      <FundingRow label={t("admin.treasury.onchainUsdt", "On-chain USDT")} value={rail.onchain_usdt ? qty(rail.onchain_usdt, locale) : undefined} />
      <FundingRow label={t("admin.treasury.gas", "Gas")} value={rail.onchain_gas ? `${qty(rail.onchain_gas, locale)} ${gasSymbol}`.trimEnd() : undefined} />
      {rail.gas_station_address && (
        <div className="space-y-1.5">
          <div className="flex items-center gap-1.5">
            {/* The accent parenthetical is its own complete thought, so it keeps its own key
                and its own colour rather than being folded into the label. */}
            <p className="text-xs text-ink-soft">
              {t("admin.treasury.gasStation", "Gas station")}{" "}
              <span className="text-positive">{t("admin.treasury.gasStationHint", "(fund {symbol} here — pays sweep gas drops)", { symbol: gasSymbol || t("admin.treasury.gasWord", "gas") })}</span>
            </p>
            <TipAnchor anchor="admin.treasury.rail.gas-station" />
          </div>
          <CopyableAddress address={show(rail.gas_station_address)} />
          <FundingRow
            label={t("admin.treasury.gasStationBalance", "Gas station balance")}
            value={rail.gas_station_gas ? `${qty(rail.gas_station_gas, locale)} ${gasSymbol}`.trimEnd() : undefined}
          />
        </div>
      )}
    </div>
  );
}

function FundingRow({ label, value }: { label: string; value: string | undefined }) {
  return (
    <div className="flex items-center justify-between text-xs">
      <span className="text-ink-soft">{label}</span>
      <span className="tabular-nums">{value ?? "—"}</span>
    </div>
  );
}

/** Address row with full address in a code block + copy button.
 *  Follows the same pattern as deposit-view's deposit address. */
function CopyableAddress({ address, label }: { address: string; label?: string }) {
  const t = useT();
  const [copied, setCopied] = useState(false);

  const copy = useCallback(() => {
    void navigator.clipboard.writeText(address);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  }, [address]);

  return (
    <div className="space-y-1">
      {label && <p className="text-xs text-ink-soft">{label}</p>}
      <div className="flex items-center gap-1.5">
        <code className="flex-1 min-w-0 truncate rounded border border-border bg-secondary px-2 py-1 font-mono-tech text-xs text-ink-soft" title={address}>
          {address}
        </code>
        <Button type="button" variant="outline" size="icon" onClick={copy} aria-label={t("admin.treasury.a11y.copy", "Copy {what}", { what: label ?? t("ui.address", "address") })}>
          {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
        </Button>
      </div>
    </div>
  );
}

/** A native-unit decimal string → grouped display; 6 dp so a thin gas balance
 * (e.g. 0.005 BNB) doesn't round to nothing.
 *
 * Grouped in the reader's locale, not `en-US`: this is a gas quantity, not money, so it
 * is outside `shared/lib/money.ts`'s fixed-precision policy and had no reason to stay
 * English. A German operator reads `1.234,5 BNB`. */
function qty(value: string, locale: Locale): string {
  const n = Number(value);
  if (!Number.isFinite(n)) return value;
  return n.toLocaleString(locale, { maximumFractionDigits: 6 });
}

