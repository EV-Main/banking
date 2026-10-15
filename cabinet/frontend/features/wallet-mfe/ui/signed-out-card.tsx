import { useLocale, useT } from "@evinvest/i18n/react";

import { Button, Card, Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyTitle } from "@evinvest/uikit";

import { cabinetPath } from "@/shared/config/base-path";

/**
 * No session on the wallet origin. Sign-in happens on evinvest.ltd, in a new tab: the host
 * page stays where it was, and when the reader comes back to it the element re-reads on
 * focus and finds the session the tab just opened (the cookie is the wallet origin's).
 */
export function SignedOutCard({ origin }: { origin: string }) {
  const t = useT();
  const locale = useLocale();
  return (
    <Card>
      <Empty>
        <EmptyHeader>
          <EmptyTitle>{t("walletMfe.signedOut", "Sign in to see your wallet")}</EmptyTitle>
          <EmptyDescription>{t("walletMfe.signedOutDesc", "Your balance and activity appear here once you are signed in to EV Invest.")}</EmptyDescription>
        </EmptyHeader>
        <EmptyContent>
          <Button asChild>
            <a href={new URL(cabinetPath(locale, "/login"), origin).href} target="_blank" rel="noopener">
              {t("walletMfe.signIn", "Sign in to EV Invest")}
            </a>
          </Button>
        </EmptyContent>
      </Empty>
    </Card>
  );
}
