// The activation funnel, as names. They are a contract shared with the site
// (site_conductor's landing steps come first) and with the PostHog funnel definition, so
// they are spelled here once and never inline — a rename here is a rename of a dashboard.
//
// Props are never PII: no email, no name, no free text, no URL (a `returnTo` is a URL).
export const ACTIVATION = {
  /** The cabinet sign-in page was rendered. */
  loginView: "login_view",
  /** This tab rendered as a signed-in user for the first time. */
  sessionCreated: "session_created",
  /** A verification attempt was opened at the vendor. */
  kycStarted: "kyc_started",
  /** The caller's tier rose from 0 to the entry tier while this tab was watching. */
  kycCompleted: "kyc_completed",
  /** The account's one and only credited deposit was seen for the first time. */
  firstDeposit: "first_deposit",
  /** A subscription was accepted by an account that held no units before it. */
  firstSubscription: "first_subscription",
} as const;

export type ActivationEvent = (typeof ACTIVATION)[keyof typeof ACTIVATION];
