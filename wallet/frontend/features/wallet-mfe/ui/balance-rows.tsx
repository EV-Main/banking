import { useLocale, useT } from "@evinvest/i18n/react";

import { ListRow, ListRows } from "@evinvest/uikit";

import { formatUsdt } from "@/shared/lib/money";

import type { WalletBalance } from "../lib/parse";

// Light DOM: a host without a CSS reset would indent the list and push values past the card.
const UNPADDED = "m-0 list-none p-0";

/**
 * The balance split. The full wallet shows all three terms that sum to the total (a surface
 * that shows the split shows all of them, `shared/contracts`); a scope host shows main and
 * the compartments — the money that host's services can reach once the user moves it in.
 */
export function BalanceRows({ balance, scope }: { balance: WalletBalance; scope: string | null }) {
  const t = useT();
  const locale = useLocale();
  const usdt = (value: string) => <span className="tabular-nums">{formatUsdt(value, locale)}</span>;

  if (scope !== null) {
    return (
      <ListRows className={UNPADDED}>
        <ListRow
          label={t("walletMfe.main", "Main account")}
          description={t("walletMfe.mainDesc", "Deposits and withdrawals go through here")}
          value={usdt(balance.available)}
        />
        {/* The BFF serves only the sum across scopes today; the per-scope line waits on
            a compartments read (ListCompartments) being routed. */}
        <ListRow
          label={t("walletMfe.compartments", "In compartments")}
          description={t("walletMfe.compartmentsDesc", "Across all services; moves to and from main are instant")}
          value={usdt(balance.inCompartments)}
        />
      </ListRows>
    );
  }

  return (
    <ListRows className={UNPADDED}>
      <ListRow label={t("walletMfe.available", "Available")} value={usdt(balance.available)} />
      <ListRow label={t("walletMfe.pendingWithdrawal", "Pending withdrawal")} value={usdt(balance.pendingWithdrawal)} />
      <ListRow label={t("walletMfe.compartments", "In compartments")} value={usdt(balance.inCompartments)} />
    </ListRows>
  );
}
