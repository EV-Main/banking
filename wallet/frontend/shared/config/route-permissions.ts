// What each cabinet section needs the caller's seat to hold — the one table the route gate
// reads. A guest holds the guest seat's permissions (`/auth/session`), so a section opens to
// guests by holding something the guest seat lists, and nothing here changes.

const OWN = {
  profile: "concierge:self:profile",
  notifications: "concierge:self:notifications",
  sessions: "concierge:self:sessions",
  wallet: "bank:self:wallet",
  invest: "bank:self:invest",
  operations: "bank:self:operations",
} as const;

/** The staff console: every seat that may read other users. */
const STAFF = "concierge:user:read";

/** Zone-relative section prefix → permission. Matched on whole segments; `/` is Home alone. */
const SECTIONS: readonly (readonly [prefix: `/${string}`, permission: string])[] = [
  ["/admin", STAFF],
  ["/consilium", STAFF],
  ["/invest", OWN.invest],
  ["/wallet", OWN.wallet],
  ["/operations", OWN.operations],
  ["/notifications", OWN.notifications],
  ["/settings", OWN.sessions],
  ["/profile", OWN.profile],
];

/** What it takes merely to have an account — a page any signed-in person may open. */
export const ACCOUNT = OWN.profile;

/** The permission a zone-relative page needs, or `null` for a page no section claims. */
export function requiredPermission(path: string): string | null {
  if (path === "/") return OWN.wallet;
  const match = SECTIONS.find(([prefix]) => path === prefix || path.startsWith(`${prefix}/`));
  return match ? match[1] : null;
}
