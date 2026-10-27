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
  // Three terms sum to `total`: `available` + `pending_withdrawal` + `in_compartments`
  // (the caller's scope compartments, still theirs). A surface that shows the split shows
  // all three.
  BankingV1Balance as Balance,
  BankingV1NetworkWithdrawable as NetworkWithdrawable,
  BankingV1DepositAddress as DepositAddress,
  BankingV1Withdrawal as Withdrawal,
  BankingV1WithdrawalList as WithdrawalList,
  BankingV1Deposit as Deposit,
  BankingV1DepositList as DepositList,
  // The unified activity timeline — one read model over deposits and withdrawals.
  BankingV1Operation as Operation,
  // One compartment's balance (`GET /api/wallet/compartment?scope=`).
  BankingV1Compartment as Compartment,
  BankingV1CompartmentList as CompartmentList,
  BankingV1CompartmentMove as CompartmentMove,
  // Payment orders are NOT re-exported from `./gen`: the BFF reshapes them (resolved ends
  // with a `detail`, lowercase states, string stamps), so the hand-written `./payments` is
  // the wallet host's contract for that surface.
  // Mandates are not re-exported either: the BFF reshapes them (lowercase operation
  // names, string stamps), so `wallet/backend/src/dto.rs` `Mandate` is their contract.
} from "./gen";

// Identity surface — owned by the concierge plane.
export type {
  ConciergeV1UserProfile as UserProfile,
  ConciergeV1Session as Session,
} from "./gen";

// Mandates: hand-written for the reason given above.
export type { Mandate, MandateList, MandateOperation, MandateRequest } from "./mandate";
