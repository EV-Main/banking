//! What every websocket bridge in this BFF shares: the handshake's cross-site guard, the
//! close codes a client keys on, the keepalive cadence and the token-expiry clock.
//!
//! A bridge is a thin pump from one upstream server-stream to one browser socket, and
//! three rules make it safe to keep open (see [`super::governance_ws`] for the first one,
//! [`super::book_ws`] for the second):
//!
//! - **It is authorized at the handshake and only until the token says so.** `ev_access`
//!   is verified exactly as on the REST routes, and the socket closes itself when that
//!   token expires rather than outliving the session that opened it.
//! - **The handshake checks `Origin`.** A websocket handshake is exempt from the
//!   same-origin policy but still carries cookies, so without this any page could open
//!   the signed-in user's feed. This is the socket's half of what CSRF double-submit does
//!   for the mutating REST routes.
//! - **It is mounted outside the request-deadline layer** (see [`crate::routes::router`]),
//!   because that layer exists to kill a wedged request and would kill this instead.

use std::time::Duration;

use axum::{
	extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket},
	http::{HeaderMap, header},
};

use crate::{state::AppState, util::now_secs};

/// Keepalive cadence. Also the worst-case lag on noticing a browser that vanished without
/// a close frame: the send that fails is how a dead peer is reaped.
pub const HEARTBEAT: Duration = Duration::from_secs(25);

/// Application close codes (the 4000–4999 range is ours). The client distinguishes "your
/// session ended, sign in again" from "the feed is down, fall back to polling".
pub const CLOSE_NORMAL: u16 = 1000;
pub const CLOSE_TOKEN_EXPIRED: u16 = 4401;
pub const CLOSE_FEED_UNAVAILABLE: u16 = 4503;

/// Whether the handshake's `Origin` is the one browser origin this BFF serves. An unset
/// `CABINET_WS_ORIGIN` disables the check for local development; production cannot reach
/// that state, because the config layer refuses to boot without the value.
pub fn origin_allowed(st: &AppState, headers: &HeaderMap) -> bool {
	let Some(expected) = st.config.cabinet_ws_origin.as_deref() else {
		return true;
	};
	origin_is(headers, expected)
}

/// Whether the request's `Origin` is exactly `expected`; a missing one is not.
pub fn origin_is(headers: &HeaderMap, expected: &str) -> bool {
	headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()).is_some_and(|origin| origin == expected)
}

/// How long the verified token still authorizes this socket. Saturating, so a token that
/// expired between the verify and here closes immediately rather than sleeping forever.
pub fn time_left(expires_at: u64) -> Duration {
	Duration::from_secs(expires_at.saturating_sub(now_secs().max(0) as u64))
}

pub async fn close(mut socket: WebSocket, code: u16, reason: &'static str) {
	// A peer that is already gone cannot be told why; there is nothing left to do with
	// the failure but return.
	let _ = socket
		.send(Message::Close(Some(CloseFrame {
			code,
			reason: Utf8Bytes::from_static(reason),
		})))
		.await;
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Pitfall 22. A token that is already expired must not buy a socket that sleeps until
	/// the far future before noticing.
	#[test]
	fn an_expired_token_closes_the_socket_at_once() {
		assert_eq!(time_left(0), Duration::ZERO);
		let soon = now_secs() as u64 + 30;
		assert!(time_left(soon) <= Duration::from_secs(30));
	}
}
