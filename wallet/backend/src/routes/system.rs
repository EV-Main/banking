use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use serde_json::json;

use crate::state::AppState;

/// `GET /api/health/live` — the process is up and serving: no upstream call, no state.
///
/// THIS is the path for the pod's kubelet probes (liveness, readiness, startup). The
/// deep smoke check [`health`] is for a human or a monitor, never for a probe: it
/// dials piggybank, so with piggybank mid-rollout it answers 502, and a probe wired to
/// it fails through no fault of the BFF. With a single BFF replica that failure has
/// no upside and one real cost — the readiness probe pulls the only endpoint off the
/// Service, the liveness probe restarts the pod after three misses, and the wallet host's
/// Next proxy meets ECONNREFUSED and answers a bodyless 500 where the BFF would have
/// answered a JSON error the UI can render. A probe must only ask "is this process
/// alive and listening", which is exactly what this endpoint answers.
pub async fn live() -> Json<serde_json::Value> {
	Json(json!({ "ok": true }))
}

/// `GET /api/health` — BFF smoke path: browser → here → piggybank `HealthService.Check`.
///
/// A dependency check, not a probe target — see [`live`] for why the kubelet must not
/// be pointed here.
pub async fn health(State(st): State<AppState>) -> Response {
	match st.grpc.check().await {
		Ok(res) => Json(json!({ "ok": true, "backend": res.status })).into_response(),
		Err(e) => {
			// Log-then-withhold (the `error.rs` discipline): a transport error can embed
			// addresses/dependency detail, so the browser gets a fixed generic string.
			tracing::warn!(code = ?e.code(), detail = %e.message(), "health check upstream error withheld from client");
			(StatusCode::BAD_GATEWAY, Json(json!({ "ok": false, "error": "upstream unavailable" }))).into_response()
		}
	}
}

/// The probe path, through the real router and its layers, with every plane unreachable.
#[cfg(test)]
mod probe_tests {
	use std::{collections::HashMap, sync::Arc};

	use axum::{
		body::{Body, to_bytes},
		http::{Request, StatusCode},
	};
	use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
	use tower::ServiceExt;

	use crate::{
		config::AppConfig,
		cookies::CookieNames,
		routes::{approval::AttemptLimiter, router},
		session::BankingTokens,
		state::{AppState, Grpc},
	};

	/// Nothing listens here; the channels are lazy, so nothing is dialled until a handler
	/// asks. A probe answer that arrives at all is proof the probe never asked.
	const BLACK_HOLE: &str = "http://127.0.0.1:1";

	fn app() -> axum::Router {
		let env = HashMap::from([
			("PIGGYBANK_GRPC_ADDR", BLACK_HOLE),
			("BANKING_AUTH_GRPC_ADDR", BLACK_HOLE),
			("CONCIERGE_GRPC_ADDR", BLACK_HOLE),
			("BANKING_ISSUANCE_TOKEN", "test-issuance"),
			("AUTH_ISSUER", "https://auth.test"),
			("AUTH_CLIENT_AUDIENCE", "concierge"),
			("APP_ENV", "development"),
		]);
		let config = AppConfig::from_source(|var| env.get(var).map(|value| (*value).to_string())).expect("the test env loads");
		let verifier = Verifier::try_new(VerifierConfig {
			issuer: config.auth_issuer.clone(),
			audiences: vec![config.auth_client_audience.clone()],
			allowed_types: vec![TokenType::Access],
			jwks_grpc_endpoint: BLACK_HOLE.into(),
		})
		.expect("build the verifier");

		router(AppState {
			cookies: Arc::new(CookieNames::new(config.cookie_secure())),
			banking: Arc::new(BankingTokens::new()),
			approvals: Arc::new(AttemptLimiter::default()),
			money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
			wallet_actions: None,
			verifier,
			grpc: Grpc::connect_lazy(BLACK_HOLE, BLACK_HOLE, BLACK_HOLE, Some("test-issuance".into())).expect("build the lazy channels"),
			config: Arc::new(config),
		})
	}

	async fn get(uri: &str) -> (StatusCode, serde_json::Value) {
		let request = Request::builder().uri(uri).body(Body::empty()).expect("build the request");
		let response = app().oneshot(request).await.expect("the router responds");
		let status = response.status();
		let body = to_bytes(response.into_body(), 1 << 16).await.expect("read the body");
		(status, serde_json::from_slice(&body).expect("a JSON body"))
	}

	/// The kubelet's question is "is the process alive" — the answer must not depend on
	/// piggybank being up, or a piggybank rollout takes the BFF's only replica with it.
	#[tokio::test]
	async fn live_answers_ok_with_every_plane_unreachable() {
		let (status, body) = get("/api/health/live").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body, serde_json::json!({ "ok": true }));
	}

	/// The contrast that makes the split necessary: the deep check does dial piggybank and
	/// reports it down — right for a smoke check, ruinous for a probe.
	#[tokio::test]
	async fn deep_health_reports_the_unreachable_plane() {
		let (status, body) = get("/api/health").await;
		assert_eq!(status, StatusCode::BAD_GATEWAY);
		assert_eq!(body["ok"], serde_json::json!(false));
	}
}
