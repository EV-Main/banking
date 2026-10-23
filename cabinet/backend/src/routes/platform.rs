//! `GET /api/platform` — the public platform status for the signed-in shell (system
//! banner + admin env badge). Session-authed for ANY role and best-effort end to end: the
//! banner must never break a page, so every upstream failure degrades to inactive defaults
//! instead of an error. Feature flags are
//! deliberately dropped — they never reach the browser through this route.

use axum::{Json, extract::State};
use axum_extra::extract::cookie::CookieJar;
use serde::Serialize;

use crate::{
	error::ApiError,
	routes::{require_money_token, require_token},
	state::AppState,
};

/// The browser-facing status: the five public platform fields plus the BFF's `APP_ENV`.
/// Composed here rather than in `dto.rs` because it is cross-plane (concierge config +
/// banking read-only + BFF env), not a passthrough of any one proto.
#[derive(Serialize)]
pub struct PlatformStatus {
	pub environment: String,
	pub maintenance_mode: bool,
	pub read_only: bool,
	pub announcement_title: String,
	pub announcement_body: String,
	pub announcement_active: bool,
}

/// `GET /api/platform` — platform status for any authenticated session.
pub async fn status(State(st): State<AppState>, jar: CookieJar) -> Result<Json<PlatformStatus>, ApiError> {
	let token = require_token(&st, &jar).await?;

	// Inactive defaults on ANY failure — including PermissionDenied while concierge still
	// gates the read to operator+ — so the route works for every role from day one.
	let config = st.grpc.platform_config(&token).await.unwrap_or_else(|s| {
		tracing::debug!(code = ?s.code(), "platform config unavailable; serving inactive defaults");
		Default::default()
	});

	// The read-only kill-switch lives on the money plane; best-effort, because investor
	// money tokens lack TreasuryRead and must read it as false.
	let read_only = match require_money_token(&st, &jar).await {
		Ok(mt) => st.grpc.operations_mode(&mt).await.map(|m| m.read_only).unwrap_or(false),
		Err(_) => false,
	};

	Ok(Json(PlatformStatus {
		environment: st.config.app_env.clone(),
		maintenance_mode: config.maintenance_mode,
		read_only,
		announcement_title: config.announcement_title,
		announcement_body: config.announcement_body,
		announcement_active: config.announcement_active,
	}))
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::PlatformStatus;

	// The banner and env badge depend on exactly this shape — and `flags` must never be
	// serialized here (the concierge config carries them; this route drops them).
	#[test]
	fn platform_status_shape_has_no_flags() {
		let got = serde_json::to_value(PlatformStatus {
			environment: "development".into(),
			maintenance_mode: false,
			read_only: true,
			announcement_title: "t".into(),
			announcement_body: "b".into(),
			announcement_active: true,
		})
		.unwrap();
		assert_eq!(
			got,
			json!({
				"environment": "development",
				"maintenance_mode": false,
				"read_only": true,
				"announcement_title": "t",
				"announcement_body": "b",
				"announcement_active": true
			})
		);
	}
}
