//! Cross-origin READS of the wallet from the scope hosts (`WALLET_CORS_ORIGINS`).
//!
//! A scope's own host (`sa.evinvest.ltd`, `invest.evinvest.ltd`) shows the user's balance
//! and history, so it has to read them with the user's cookies. It must never act: every
//! mutating wallet route is first-party only (`wallet.proto`, plan §3.9), and the way to
//! keep that true is that no mutating route ever answers a cross-origin request in a way
//! a browser will let the caller see. So CORS is opted into per route, here, on a few
//! reads — never as a router-wide layer that a new POST would silently inherit.
//!
//! Only simple requests are served: a `GET` with cookies needs no preflight, so `OPTIONS`
//! is not answered and a cross-origin read that adds a non-safelisted header fails in the
//! browser instead of widening what a scope host may send.

use axum::{
	extract::{Request, State},
	http::{HeaderValue, Method, header},
	middleware::{self, Next},
	response::Response,
	routing::MethodRouter,
};

use crate::state::AppState;

/// Make `route` readable from the configured scope hosts.
pub fn scope_readable(state: &AppState, route: MethodRouter<AppState>) -> MethodRouter<AppState> {
	route.route_layer(middleware::from_fn_with_state(state.clone(), allow_scope_origin))
}

async fn allow_scope_origin(State(st): State<AppState>, request: Request, next: Next) -> Response {
	let allowed = matches!(*request.method(), Method::GET | Method::HEAD)
		.then(|| request.headers().get(header::ORIGIN))
		.flatten()
		.filter(|origin| st.config.wallet_cors_origins.iter().any(|allowed| origin.as_bytes() == allowed.as_str().as_bytes()))
		.cloned();
	let mut response = next.run(request).await;
	let headers = response.headers_mut();
	// The body differs by `Origin` (the CORS headers do), so a cache keyed on the URL alone
	// must not hand one host's answer to another.
	headers.append(header::VARY, HeaderValue::from_static("origin"));
	if let Some(origin) = allowed {
		headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
		headers.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
	}
	response
}
