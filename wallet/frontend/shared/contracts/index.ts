// Clean re-exports of the proto-derived types — import from `@/shared/contracts`,
// never `./gen` directly.
//
// The backend gRPC proto (`contracts/proto`) is the single source of truth:
// `protoc-gen-connect-openapi` emits `contracts/openapi.json` from it, and
// `@hey-api/openapi-ts` emits `./gen` from that (`npm run gen:api`, or the flake
// `nix run .#gen-api`). Field names are snake_case to match the BFF's `keepCase`
// proto-loader shape, and every field is optional (proto3 JSON semantics).
//
// Money types come from banking's own proto (`BankingV1*`). The identity surface
// (profile + sessions) is served by the concierge plane, so those types come from
// concierge's OWN proto (`ConciergeV1*`) — the actual source of that wire data —
// rather than banking's byte-identical copy, so a concierge-side field change is
// surfaced by `gen-api` instead of going silently stale.

export type {
  BankingV1PayableInvoice as PayableInvoice,
  BankingV1Wallet as Wallet,
  // Five terms sum to `total`: `available` + `in_orders` (cash escrowed by resting buy
  // orders on the book) + `invested` (units at NAV, those escrowed by resting sells
  // included) + `pending_withdrawal` + `in_compartments` (the caller's scope
  // compartments, still theirs). A surface that shows the split shows all five.
  BankingV1Balance as Balance,
  BankingV1NetworkWithdrawable as NetworkWithdrawable,
  BankingV1DepositAddress as DepositAddress,
  BankingV1Withdrawal as Withdrawal,
  BankingV1WithdrawalList as WithdrawalList,
  BankingV1Deposit as Deposit,
  BankingV1DepositList as DepositList,
  // Fund shares (the service currency).
  BankingV1FundNav as FundNav,
  BankingV1Subscription as Subscription,
  BankingV1Redemption as Redemption,
  // The unified activity timeline — one read model over all four money kinds.
  BankingV1Operation as Operation,
  // One compartment's balance (`GET /api/wallet/compartment?scope=`).
  BankingV1Compartment as Compartment,
  BankingV1CompartmentList as CompartmentList,
  BankingV1CompartmentMove as CompartmentMove,
  // What a fund charges, and what the caller's holding owes for it right now.
  BankingV1FeePolicy as FeePolicy,
  // Payment orders are NOT re-exported from `./gen`: the BFF reshapes them (resolved ends
  // with a `detail`, a nested consent, lowercase states, string stamps), so the
  // hand-written `./payments` is the wallet host's contract for that surface.
  // Mandates are not re-exported either: the BFF reshapes them (lowercase operation
  // names, string stamps), so `wallet/backend/src/dto.rs` `Mandate` is their contract.
} from "./gen";

// The allocation catalog is served to BOTH the investor surface (`/api/allocations`,
// open products only) and the admin one from the same BFF DTO, so the hand-written
// types live in one place and are re-exported here for the investor imports.
//
// The treasury and the cap table (#245) are hand-written there too, NOT re-exported from
// `./gen`: the BFF reshapes them (`nav_posted_at_unix` → the string `nav_posted_at`, a
// missing claim → zeros, a holder-less line dropped), so the generated `BankingV1Treasury`
// is not the shape that arrives. `./governance` carries the two #245 consilium terms for
// the same reason.
export type {
  Allocation,
  AllocationAccessGrant,
  AllocationAccessGrantList,
  AllocationAccessLevel,
  AllocationBacking,
  AllocationClaim,
  AllocationGrantLevel,
  AllocationIcon,
  AllocationList,
  AllocationState,
  AllocationTreasury,
  RailLiquidity,
  SeedCapitalBody,
  SeedCapitalProposal,
  Treasury,
  UnitHolderKind,
  UnitHolderRef,
  UnitHolding,
} from "./admin";

// Identity surface — owned by the concierge plane.
export type {
  ConciergeV1UserProfile as UserProfile,
  ConciergeV1Session as Session,
} from "./gen";

// Mandates: hand-written for the reason given above.
export type { GrantMandateBody, Mandate, MandateList, MandateOperation, MandateRequest } from "./mandate";
