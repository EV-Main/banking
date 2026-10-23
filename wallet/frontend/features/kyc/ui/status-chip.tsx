"use client";

import type { Translate } from "@evinvest/i18n";
import { useT } from "@evinvest/i18n/react";

import { Skeleton } from "@evinvest/uikit";

import { profileResource } from "@/entities/user/model/profile-resource";
import { type KycChip, kycChipState } from "@/features/kyc/lib/chip-state";
import { useKycStatus } from "@/features/kyc/model/use-kyc-status";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { Pill } from "@/shared/ui/list-card";

// The standing answer to "am I verified?" (#395), on the home host's verification card. It
// shows all four states.
//
// Nothing is drawn until the state is known — neither while the reads are in flight nor
// after both have failed. `useKycStatus` answers `level: 0` in both, and a chip that took
// that for tier 0 would paint "Not verified" at every verified reader for a beat on every
// cold load, and for the length of any outage — the flash this whole surface must not have.
// "Settled" means the plane answered, or the profile did.

function useKycChip() {
  const { level, runningCase, known, loading } = useKycStatus();
  const { data: profile } = useResource(profileResource);
  return { loading, chip: kycChipState({ level, runningCase, settled: known || profile != null }) };
}

const chipLabels = (t: Translate): Record<KycChip["labelKey"], string> => ({
  "kyc.chip.notStarted": t("kyc.chip.notStarted", "Not verified"),
  "kyc.chip.review": t("kyc.chip.review", "In review"),
  "kyc.chip.verified": t("kyc.chip.verified", "Verified"),
  "kyc.chip.attention": t("kyc.chip.attention", "Needs attention"),
});

export function KycStatusChip({ className }: { className?: string }) {
  const t = useT();
  const { loading, chip } = useKycChip();
  // As wide as the widest of the five translations of the longest label, so the row does not
  // shift when the state lands.
  if (loading) return <Skeleton className={cn("h-5 w-24 shrink-0 rounded-full", className)} />;
  if (chip === null) return null;
  return (
    // i18n-max: 15 — a `shrink-0` pill beside a `min-w-0` row label.
    <Pill tone={chip.tone} className={cn("shrink-0", className)}>
      <span data-state={chip.state}>{chipLabels(t)[chip.labelKey]}</span>
      {/* The only timing wording the owner approved (`kyc.dialog.timeBody`). The pill has
          room for the state alone, so the ETA rides along for assistive tech: the pill is
          not focusable, so a tooltip would have nothing to open on. */}
      {chip.state === "review" && <span className="sr-only">{t("kyc.chip.reviewEta", "Most checks are decided within the hour.")}</span>}
    </Pill>
  );
}

