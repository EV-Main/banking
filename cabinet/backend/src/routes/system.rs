use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{error::ApiError, state::AppState};

/// `GET /api/health/live` — the process is up and serving: no upstream call, no state.
///
/// THIS is the path for the pod's kubelet probes (liveness, readiness, startup). The
/// deep smoke check [`health`] is for a human or a monitor, never for a probe: it
/// dials piggybank, so with piggybank mid-rollout it answers 502, and a probe wired to
/// it fails through no fault of the BFF. With a single BFF replica that failure has
/// no upside and one real cost — the readiness probe pulls the only endpoint off the
/// Service, the liveness probe restarts the pod after three misses, and the cabinet's
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

/// A microfrontend registry entry — the strict, validated shape served to the browser.
/// `deny_unknown_fields` rejects a poisoned registry that carries extra/typo'd keys, and
/// the origin/integrity gate in [`validate`] rejects off-allow-list or hash-less remotes
/// before any entry reaches the host that injects it as first-party `<script>`.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MfeEntry {
	pub name: String,
	pub tag: String,
	#[serde(rename = "scriptUrl")]
	pub script_url: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub integrity: Option<String>,
	pub kind: MfeKind,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MfeKind {
	Component,
	Page,
}

/// `GET /api/mfe-registry` — serve the microfrontend registry to the browser. The host
/// resolves each `<RemoteElement>` against this, so remotes deploy independently. The
/// payload is parsed into [`MfeEntry`] and validated against the origin allow-list before
/// it is served — a malformed or off-allow-list registry fails here, never at the browser.
pub async fn mfe_registry(State(st): State<AppState>) -> Result<Json<Vec<MfeEntry>>, ApiError> {
	let raw = tokio::fs::read(&st.config.mfe_registry_path)
		.await
		.map_err(|e| ApiError::Internal(format!("mfe registry unavailable: {e}")))?;
	let entries: Vec<MfeEntry> = serde_json::from_slice(&raw).map_err(|e| ApiError::Internal(format!("mfe registry invalid: {e}")))?;
	validate(&entries, &st.config.mfe_allowed_origins).map_err(ApiError::Internal)?;
	Ok(Json(entries))
}

/// Reject any entry whose bundle origin is not same-origin (relative) or on the allow-list,
/// and require an SRI hash for cross-origin bundles (delivered atomically with the URL).
fn validate(entries: &[MfeEntry], allowed_origins: &[String]) -> Result<(), String> {
	for (i, entry) in entries.iter().enumerate() {
		match origin_of(&entry.script_url) {
			// Relative URL ⇒ same-origin (the cabinet itself), already constrained by 'self'.
			None => {}
			Some(origin) => {
				if !allowed_origins.iter().any(|o| o == &origin) {
					return Err(format!("mfe registry entry {i} ({}): origin {origin} not on the allow-list", entry.name));
				}
				if entry.integrity.as_deref().is_none_or(|h| !h.starts_with("sha")) {
					return Err(format!("mfe registry entry {i} ({}): cross-origin bundle requires an SRI integrity hash", entry.name));
				}
			}
		}
	}
	Ok(())
}

/// The `scheme://host[:port]` origin of an absolute http(s) URL, or `None` for a relative
/// (same-origin) URL. Returns an error string only for an absolute URL we refuse to parse.
fn origin_of(script_url: &str) -> Option<String> {
	let (scheme, rest) = script_url.split_once("://")?; // relative URL → same-origin → None.
	let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
	Some(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn entry(script_url: &str, integrity: Option<&str>) -> MfeEntry {
		MfeEntry {
			name: "x".into(),
			tag: "mfe-x".into(),
			script_url: script_url.into(),
			integrity: integrity.map(str::to_string),
			kind: MfeKind::Component,
		}
	}

	#[test]
	fn same_origin_relative_bundle_passes_without_integrity() {
		let entries = [entry("/mfe/x.js", None)];
		assert!(validate(&entries, &[]).is_ok());
	}

	#[test]
	fn off_allow_list_origin_is_rejected() {
		let entries = [entry("https://evil.example/x.js", Some("sha384-abc"))];
		let err = validate(&entries, &["https://cdn.trusted.example".into()]).unwrap_err();
		assert!(err.contains("not on the allow-list"), "{err}");
	}

	#[test]
	fn allow_listed_origin_without_integrity_is_rejected() {
		let entries = [entry("https://cdn.trusted.example/x.js", None)];
		let err = validate(&entries, &["https://cdn.trusted.example".into()]).unwrap_err();
		assert!(err.contains("SRI integrity hash"), "{err}");
	}

	#[test]
	fn allow_listed_origin_with_integrity_passes() {
		let entries = [entry("https://cdn.trusted.example/x.js", Some("sha384-abc"))];
		assert!(validate(&entries, &["https://cdn.trusted.example".into()]).is_ok());
	}

	#[test]
	fn malformed_registry_fails_to_deserialize() {
		// Unknown field (deny_unknown_fields) and missing required field both fail parsing.
		assert!(serde_json::from_str::<Vec<MfeEntry>>(r#"[{"name":"x","tag":"t","scriptUrl":"/x.js","kind":"page","evil":1}]"#).is_err());
		assert!(serde_json::from_str::<Vec<MfeEntry>>(r#"[{"name":"x","tag":"t","kind":"page"}]"#).is_err());
	}

	#[test]
	fn origin_parsing_handles_ports_and_paths() {
		assert_eq!(origin_of("https://cdn.example:8443/a/b.js?v=1"), Some("https://cdn.example:8443".into()));
		assert_eq!(origin_of("/mfe/x.js"), None);
		assert_eq!(origin_of("mfe/x.js"), None);
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
			("MFE_REGISTRY_PATH", "/mfe-registry.json"),
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
