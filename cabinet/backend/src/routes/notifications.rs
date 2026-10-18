//! Notification routes — the cabinet's view of the concierge notification plane.
//!
//! Every handler forwards the caller's own concierge token and names no subscriber:
//! concierge resolves the subscriber from the token, so there is no id in any request
//! here and therefore no way for one user to read or mutate another's inbox.
//!
//! Reads are plain GETs; every mutation is CSRF-checked, and the preference writes
//! return the full settings snapshot so the client re-renders from one authoritative
//! read rather than patching local state.

use axum::{
	Json,
	body::Bytes,
	extract::{Query, State},
	http::HeaderMap,
};
use axum_extra::extract::cookie::CookieJar;
use evconcierge_contracts::concierge::v1 as cc;
use serde::Deserialize;

use crate::{
	dto,
	error::ApiError,
	routes::{parse_body, require_fresh_login, require_identity, require_token, verify_csrf},
	state::AppState,
	util::now_secs,
};

/// `bank:wallet-security` (`domain::scope_operations::WALLET_SECURITY_TOPIC`): spelled out here
/// because the BFF does not depend on `domain`.
const WALLET_SECURITY_TOPIC: &str = "bank:wallet-security";

#[derive(Deserialize)]
pub struct ListQuery {
	#[serde(default)]
	cursor: Option<String>,
	#[serde(default)]
	limit: Option<u32>,
	#[serde(default)]
	filter: Option<String>,
	#[serde(default)]
	topic: Option<String>,
}

/// `GET /api/notifications` — one page of the caller's inbox, newest first.
pub async fn list(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ListQuery>) -> Result<Json<dto::NotificationList>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let req = cc::ListNotificationsRequest {
		cursor: q.cursor.unwrap_or_default(),
		limit: q.limit.unwrap_or(0),
		// The UI speaks in filter names; the wire has a boolean. Anything unrecognised
		// means "no filter" rather than an error — a stale bookmark should still load.
		unread_only: q.filter.as_deref() == Some("unread"),
		topic: q.topic.unwrap_or_default(),
	};
	let list = st.grpc.list_notifications(&token, req).await.map_err(|s| ApiError::read(s, "notifications unavailable"))?;
	Ok(Json(list.into()))
}

/// `GET /api/notifications/unread-count` — the badge. Polled, so kept as narrow as possible.
pub async fn unread_count(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::UnreadCount>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let count = st.grpc.unread_notifications(&token).await.map_err(|s| ApiError::read(s, "notifications unavailable"))?;
	Ok(Json(count.into()))
}

/// `POST /api/notifications/read` — mark specific ids, or everything unread.
pub async fn mark_read(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::MarkReadResult>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let req = cc::MarkReadRequest {
		ids: v
			.get("ids")
			.and_then(|x| x.as_array())
			.map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect())
			.unwrap_or_default(),
		all: v.get("all").and_then(|x| x.as_bool()).unwrap_or(false),
	};
	Ok(Json(st.grpc.mark_notifications_read(&token, req).await?.into()))
}

/// `GET /api/notifications/settings` — channels + the topic catalogue with the
/// caller's stance on each. The catalogue is server-owned, so the client renders
/// whatever concierge lists rather than hardcoding topics.
pub async fn settings(State(st): State<AppState>, jar: CookieJar) -> Result<Json<dto::NotificationSettings>, ApiError> {
	let token = require_token(&st, &jar).await?;
	let settings = st.grpc.notification_settings(&token).await.map_err(|s| ApiError::read(s, "notification settings unavailable"))?;
	Ok(Json(settings.into()))
}

/// `POST /api/notifications/settings/channel` — flip a master channel switch.
/// Both channels may legitimately end up off; that is not an error here.
pub async fn set_channel(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::NotificationSettings>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let enabled = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
	let channel = match v.get("channel").and_then(|x| x.as_str()) {
		Some("in_app") => cc::Channel::InApp,
		Some("email") => cc::Channel::Email,
		_ => return Err(ApiError::BadRequest("channel must be in_app or email".into())),
	};
	// Switching the email channel off silences the wallet-security warnings with it, so it
	// takes the same fresh sign-in a withdrawal does: a stolen session must not be able to
	// mute the one message that would tell the owner.
	if channel == cc::Channel::Email && !enabled {
		require_fresh_login(&require_identity(&st, &jar).await?.1, now_secs().max(0) as u64)?;
	}
	let req = cc::SetChannelEnabledRequest { channel: channel as i32, enabled };
	Ok(Json(st.grpc.set_notification_channel(&token, req).await?.into()))
}

/// `POST /api/notifications/settings/topic` — follow/unfollow a topic, and choose
/// whether it also arrives by email.
pub async fn set_topic(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::NotificationSettings>, ApiError> {
	if !verify_csrf(&st, &jar, &headers) {
		return Err(ApiError::Csrf);
	}
	let token = require_token(&st, &jar).await?;
	let v = parse_body(&body);
	let Some(topic) = v.get("topic").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) else {
		return Err(ApiError::BadRequest("topic is required".into()));
	};
	let subscribed = v.get("subscribed").and_then(|x| x.as_bool()).unwrap_or(false);
	let email_enabled = v.get("email_enabled").and_then(|x| x.as_bool()).unwrap_or(true);
	// Unsubscribing from, or dropping the email copy of, the wallet-security topic is the same
	// muting as switching the email channel off.
	if topic == WALLET_SECURITY_TOPIC && (!subscribed || !email_enabled) {
		require_fresh_login(&require_identity(&st, &jar).await?.1, now_secs().max(0) as u64)?;
	}
	let req = cc::SetTopicSubscriptionRequest {
		topic: topic.to_string(),
		subscribed,
		// Defaults to true so "follow this fund" from a product page is one click:
		// following implies wanting the email copy unless the user says otherwise.
		email_enabled,
	};
	Ok(Json(st.grpc.set_notification_topic(&token, req).await?.into()))
}
