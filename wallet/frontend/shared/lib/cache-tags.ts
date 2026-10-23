// The invalidation vocabulary shared by every cached read and every mutation that moves it.
//
// A tag names a fact about the account, not an endpoint: `wallet` is "what this account
// holds", and a withdrawal, a subscription and a redemption all move it. Naming facts is
// what lets a mutation say what it changed without knowing which screens are mounted —
// `submitWithdrawal` names wallet · withdrawals · operations and every open surface reading
// any of the three refreshes itself.
//
// Kept in one file because the interesting cases cross entity boundaries: subscribing to a
// fund moves the wallet AND the positions AND the timeline. A per-entity constant would put
// the three-way relationship nowhere.

export const TAG = {
  /** Balance and holdings. */
  wallet: "wallet",
  /** The caller's own withdrawal requests. */
  withdrawals: "withdrawals",
  /** The caller's own deposits. */
  deposits: "deposits",
  /** Fund positions — units, cost basis, P&L. */
  positions: "positions",
  /** The caller's own redemption requests. */
  redemptions: "redemptions",
  /** The registry of investable funds. Operator-owned; changes rarely. */
  catalog: "catalog",
  /** Per-fund NAV. Moves only when an operator posts a valuation. */
  nav: "nav",
  /** A fund's fee terms and what the caller's holding has accrued against them. Moves
   *  when the sweeper charges, so it travels with `positions`. */
  fees: "fees",
  /** The merged activity timeline. Every money movement lands here. */
  operations: "operations",
  /** The caller's own profile. */
  profile: "profile",
  /** Notification list and per-topic settings. */
  notifications: "notifications",
  /** The caller's active device sessions. */
  sessions: "sessions",

  // ── the book (secondary market in an allocation's units) ──────────────────────
  /** A product's public book: the snapshot and the trade tape. */
  book: "book",
  /** The caller's own orders — resting, history and fills. Moves on every fill, and a
   *  fill also moves `wallet` and `positions` (the escrow settles into both), so the
   *  socket names all three when `orders_revision` moves. */
  orders: "book.orders",
  /** A product's trading terms: open flag, taker fee, tick, lot, slippage. Operator-set;
   *  changes rarely. */
  bookPolicy: "book.policy",

  // ── fund governance ───────────────────────────────────────────────────────────
  // Three facts, not three endpoints, and they move together often enough that one
  // mutation names all three at once: a removal that carries changes the roster, and a
  // roster that shrinks past the floor changes whether a payout can be opened at all.
  /** Who the fund's owners are, and whether the roster is below the payout floor. */
  owners: "owners",
  /** Open and settled owner-removal proposals, with their per-peer votes. */
  removals: "owners.removals",
  /** Open and settled owner-admission proposals, with their per-peer votes. An admission
   *  that carries GRANTS a seat, so it moves `owners` too — and a roster that grows past
   *  the payout floor changes what the payout card may offer. */
  admissions: "owners.admissions",
  /** Open and settled USER proposals — a permanent suspension, its undo, or the admin
   *  seat — with their per-owner votes. A suspension or reinstatement that carries changes
   *  a user's standing, so it moves `adminUsers` too; an admin admission that carries
   *  changes their role, which the same list shows. */
  userProposals: "owners.proposals",
  /** Payout consilia and their tallies. */
  consilium: "consilium",
  /** Payment orders — every tier, both requirements. An order that opens a consilium
   *  moves `consilium` too, and one that executes debits a claim, so the mutations name
   *  this beside `consilium` and `adminTreasury`. */
  payments: "payments",

  // ── operator console ──────────────────────────────────────────────────────────
  /** The parked outbox rows an operator unparks. Fleet health went to Grafana. */
  adminOutbox: "admin.outbox",
  /** Custody and claim balances across the rails. */
  adminTreasury: "admin.treasury",
  /** Platform config: maintenance, read-only, announcement, flags. */
  adminCabinet: "admin.cabinet",
  /** Which rails are run and which an operator froze. */
  adminRails: "admin.rails",
  /** The withdrawal and redemption queues an operator works through. */
  adminQueue: "admin.queue",
  /** The operator allocation registry — drafts and closed products included. */
  adminAllocations: "admin.allocations",
  /** Per-service investor access grants — who is raised above a product's default. */
  adminAllocationGrants: "admin.allocations.grants",
  /** Per-tenant panel access — the holders of grants in a tenant's namespace, kept by the
   *  identity plane. Separate from the grants above, which gate money, not the panel. */
  grantHolders: "admin.tenants.grants",
  /** Per-service cap table (holders and supply). An in-kind issuance moves this AND `nav`
   *  (the mark's `units_outstanding` is the same ledger fact), so
   *  the mutation names both rather than this one alone. */
  adminUnitHolders: "admin.allocations.holders",
  /** The user directory and a single user's detail. */
  adminUsers: "admin.users",
  /** The `fee` allocation's treasury view: its claim, supply, price and holders. */
  adminRevenue: "admin.revenue",
  /** Fee terms, accumulated units and the charge history. Settling moves the fee units
   *  AND the revenue figure, so a settle names this and `adminRevenue` together. */
  adminFees: "admin.fees",
} as const;
