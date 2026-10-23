"use client";

import { useT } from "@evinvest/i18n/react";

import { KycStatusChip, StartVerificationRow } from "@/features/kyc";
import { Link } from "@/shared/ui/zone-link";
import { Chevron, ListCard, ListCardTitle, ROW_INTERACTIVE, Row, RowLabel } from "@/shared/ui/list-card";
import { PageFrame } from "@/shared/ui/page-frame";

// Only the two things banking still owns for a signed-in person: their verification
// standing, and the way to their wallet. `StartVerificationRow` hides itself once the tier
// moves, so the Didit return lands on a card that already reads "Verified".
export function HomeView() {
  const t = useT();
  return (
    <PageFrame title={t("nav.group.fund", "Your account")} width="content">
      <ListCard>
        <ListCardTitle sub={t("profile.identityAndStanding", "Identity and account standing")}>{t("profile.identityVerification", "Identity verification")}</ListCardTitle>
        <Row>
          <RowLabel title={t("ui.kycLevel", "KYC level")} />
          <KycStatusChip />
        </Row>
        <StartVerificationRow />
      </ListCard>
      <ListCard>
        <Link href="/wallet" className={ROW_INTERACTIVE}>
          <RowLabel title={t("nav.wallet", "Wallet")} />
          <Chevron />
        </Link>
      </ListCard>
    </PageFrame>
  );
}
