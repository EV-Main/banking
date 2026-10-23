use axum::{Json, extract::State};
use axum_extra::extract::cookie::CookieJar;

use crate::{dto, error::ApiError, routes::require_token, state::AppState};

/// `GET /api/users` — the caller's identity and profile, from concierge's directory.
pub async fn get_me(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::UserProfile>, ApiError> {
	let token = require_token(&st, &jar).await?;
	Ok(Json(st.grpc.get_me(&token).await?.into()))
}
