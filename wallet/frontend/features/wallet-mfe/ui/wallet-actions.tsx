import { useLocale, useT } from "@evinvest/i18n/react";

import { Button } from "@evinvest/uikit";

import { zonePagePath } from "@/shared/config/base-path";

import type { WalletAction } from "../lib/wallet-action";

/**
 * The money actions. Each is a button, not a link: what happens is the host's to decide
 * (`ev:wallet-action` → a popup on evinvest.ltd) and only a click left alone navigates.
 * Deposit is not an action in that contract — it moves nothing, it shows an address — so it
 * is a plain link, and only in the full wallet: on a scope host money enters through main.
 */
export function WalletActions({ origin, scope, onAction }: { origin: string; scope: string | null; onAction: (action: WalletAction) => void }) {
  const t = useT();
  const locale = useLocale();
  return (
    <div className="flex flex-wrap gap-2">
      {scope === null && (
        <Button size="sm" href={new URL(zonePagePath(locale, "/wallet/deposit"), origin).href}>
          {t("ui.deposit", "Deposit")}
        </Button>
      )}
      <Button type="button" size="sm" variant={scope === null ? "outline" : "primary"} onClick={() => onAction("move")}>
        {t("walletMfe.move", "Move funds")}
      </Button>
      <Button type="button" size="sm" variant="outline" onClick={() => onAction("withdraw")}>
        {t("ui.withdraw", "Withdraw")}
      </Button>
    </div>
  );
}
