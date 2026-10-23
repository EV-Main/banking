// The slice's whole contract: the places a user can be offered verification — the row on the
// home host's verification card, and the block a money screen shows instead of a surface the
// hub has closed to them — plus the card's standing status chip.
//
// Everything else stays internal, `VerificationDialog` included: the presentations above are
// the only things that open it. The client and the start hook stay internal too — a second
// caller of `startVerification` would be a second place deciding what a 503 looks like,
// which is exactly what these presentations exist to share instead.
export { StartVerificationRow } from "@/features/kyc/ui/start-verification-row";
export { KycStatusChip } from "@/features/kyc/ui/status-chip";
export { VerificationRequired } from "@/features/kyc/ui/verification-required";
// The trust line beside a money input (#385): what verification protects, said next to the
// figure it protects rather than in a footer the wallet host does not have.
export { CustodyNote } from "@/features/kyc/ui/custody-note";
