/**
 * Where a cabinet user is sent when the product cannot finish something on its own.
 *
 * A constant rather than a value read from `config`: this exists precisely as the fallback
 * for a response that was SUPPOSED to carry an address and did not, so sourcing it from
 * anything that can itself be unset would rebuild the dead end it closes. A screen that
 * says "we can't do this" and then shows no way to reach anyone is worse than the refusal.
 *
 * This mailbox rather than an invented `support@`: `admin@evinvest.ltd` is the one the
 * operators already run — it is `MAIL_TEAM` and the sender on the public site — and raising
 * a KYC tier by hand from the admin console is currently the ONLY way it happens. Self-serve
 * verification is deployed but unconfigured — the concierge pod carries no `DIDIT_*`, so
 * `/kyc/start` degrades to `kyc_unavailable` before it ever dials the vendor, and the manual
 * path is the live one.
 */
export const SUPPORT_EMAIL = "admin@evinvest.ltd";
