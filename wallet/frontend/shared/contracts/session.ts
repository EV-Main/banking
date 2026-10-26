// The site-root session shape `/api/auth/session` returns. Ingress sends `/api/auth/*`
// straight to concierge, whose `SessionUser` carries `#[serde(rename_all = "camelCase")]` —
// hence camelCase here, unlike the BFF's snake_case DTOs.

export interface SessionUser {
  userId: string;
  email: string;
  /** Whether the mailbox is proven. A KYC level above 0 needs it. */
  emailVerified: boolean;
  /** A handle beside the email; absent when the account has none. */
  username?: string;
  status: string;
  role: string;
  isAdmin: boolean;
  /**
   * `role` above came from the `OWNER_SUBJECTS` emergency allowlist, not from the
   * persisted `users.role`.
   *
   * True only while the platform has no persisted owner. Render it as a warning, never as
   * ownership: it is real authority over the console and it seats nobody.
   */
  roleIsBreakGlass: boolean;
}

export interface SessionInfo {
  authenticated: boolean;
  user?: SessionUser;
  /**
   * What the caller's seat holds, concrete — a guest's set when nobody is signed in. What a
   * page shows is decided from this (`shared/config/route-permissions.ts`); the BFF still
   * decides what runs.
   */
  permissions: string[];
}
