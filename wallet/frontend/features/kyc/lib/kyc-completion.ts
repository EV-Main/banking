// When does a tier this tab has just read count as "verification completed"?
//
// The cabinet only ever sees tiers, never verdicts: `/kyc/status` drops a case the moment
// it resolves, and the mirrored `kyc_level` simply moves. So completion is a transition,
// and a transition needs a "before". Two befores are trusted: a tier of 0 read earlier in
// this page's life (the profile poll, or an operator raising the tier by hand while the
// tab sat open), and the pending mark `kyc_started` leaves behind — which is what survives
// the round-trip to the vendor's page, since that navigation unloads everything else.
//
// A tab that first opens on an already-verified account has neither, and records
// nothing: it did not watch the step happen, and guessing would count every returning
// verified user as a fresh completion.

import { ENTRY_TIER } from "../../../entities/user/lib/kyc.ts";

export function completesVerification(before: number | null, level: number, pending: boolean): boolean {
  return level >= ENTRY_TIER && (before === 0 || pending);
}
