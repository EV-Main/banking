/// The cookies the BFF READS — auth is shell-owned, so the BFF sets none. Both are
/// minted by the concierge auth web surface on the shared origin and arrive here
/// first-party through the conductor's zone mount. Names are `__Host-`-prefixed in
/// production (HTTPS) and bare in local dev, matching the shell's cookie identity
/// and the frontend's `shared/config/cookies.ts` / `shared/lib/csrf-client.ts`.
#[derive(Clone)]
pub struct CookieNames {
	/// The short-TTL concierge access JWT (`Path=/`) — the request credential,
	/// verified locally against the concierge JWKS before any privileged use.
	pub access: String,
	/// The readable double-submit token paired with the shell session; mutating
	/// routes require it echoed in `x-ev-csrf`.
	pub csrf: String,
}

impl CookieNames {
	pub fn new(secure: bool) -> Self {
		let prefix = if secure { "__Host-" } else { "" };
		Self {
			access: format!("{prefix}ev_access"),
			csrf: format!("{prefix}ev_csrf"),
		}
	}
}
