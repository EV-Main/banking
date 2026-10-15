// The two reads the wallet element shows, parsed at the edge. The element runs on hosts that
// ship on their own releases, against a BFF that ships on ours, so a body is read field by
// field and anything malformed reads as absent instead of being trusted through a cast.
//
// Pure and import-free so `node --test` runs it as is.

/** Decimal USDT strings, exactly as the wire sent them — display only. */
export interface WalletBalance {
  available: string;
  inOrders: string;
  invested: string;
  pendingWithdrawal: string;
  inCompartments: string;
  total: string;
}

export interface WalletOperation {
  id: string;
  kind: string;
  state: string;
  /** Unix seconds; 0 when the hub never stamped one. */
  createdAt: number;
  /** A magnitude; the sign is the kind's. Empty for an unsettled redemption. */
  amount: string;
}

export interface WalletHistory {
  operations: WalletOperation[];
  /** The history is longer than what came back. */
  truncated: boolean;
}

// A wire decimal: optional sign, digits, optional fraction. Anything else is not shown as money.
const DECIMAL = /^-?\d+(?:\.\d+)?$/;

function field(source: unknown, key: string): unknown {
  return typeof source === "object" && source !== null ? Reflect.get(source, key) : undefined;
}

function decimal(raw: unknown): string {
  return typeof raw === "string" && DECIMAL.test(raw) ? raw : "0";
}

function text(raw: unknown, max = 128): string {
  return typeof raw === "string" && raw.length <= max ? raw : "";
}

/** The BFF renders i64 seconds as text so no client has to survive 2^53. */
function unixSeconds(raw: unknown): number {
  const n = typeof raw === "number" ? raw : typeof raw === "string" ? Number(raw) : NaN;
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}

/** `GET /api/wallet` → the balance split, or null when the body carries none. */
export function parseWalletBalance(body: unknown): WalletBalance | null {
  const balance = field(body, "balance");
  if (typeof balance !== "object" || balance === null) return null;
  return {
    available: decimal(field(balance, "available")),
    inOrders: decimal(field(balance, "in_orders")),
    invested: decimal(field(balance, "invested")),
    pendingWithdrawal: decimal(field(balance, "pending_withdrawal")),
    inCompartments: decimal(field(balance, "in_compartments")),
    total: decimal(field(balance, "total")),
  };
}

// More rows than the element ever asks for means the body is not what it asked for.
const MAX_OPERATIONS = 200;

/** `GET /api/operations` → the timeline, newest first; rows without an id are dropped. */
export function parseWalletHistory(body: unknown): WalletHistory {
  const raw = field(body, "operations");
  const rows = Array.isArray(raw) ? raw.slice(0, MAX_OPERATIONS) : [];
  const operations: WalletOperation[] = [];
  for (const row of rows) {
    const id = text(field(row, "id"), 256);
    if (id === "") continue;
    const amount = field(row, "amount");
    operations.push({
      id,
      kind: text(field(row, "kind")),
      state: text(field(row, "state")),
      createdAt: unixSeconds(field(row, "created_at")),
      amount: typeof amount === "string" && DECIMAL.test(amount) ? amount : "",
    });
  }
  return { operations, truncated: field(body, "truncated") === true };
}

/** `GET /api/auth/session` → whether the shell still knows a signed-in user. */
export function parseSignedIn(body: unknown): boolean {
  return field(body, "authenticated") === true;
}
