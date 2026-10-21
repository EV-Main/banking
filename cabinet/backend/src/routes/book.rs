//! The book routes — the secondary market in an allocation's units, behind `/api/book/*`.
//!
//! Every handler forwards the banking money token (the book is a money-plane surface:
//! an order is an escrow inside the ledger). Reads answer with a fixed generic message on
//! failure, mutations relay the hub's client-safe wording — the same split as the money
//! routes. The string vocabularies a browser may send (`side`, `kind`, `tif`,
//! `resolution`) are checked here against `evbanking_contracts::book` so a client bug is
//! refused one hop away with the list of words that would have worked, before a money
//! token is minted or the hub is called.
//!
//! The live feed is [`super::book_ws`].

use axum::{
	Json,
	body::Bytes,
	extract::{Query, State},
	http::HeaderMap,
};
use axum_extra::extract::cookie::CookieJar;
use evbanking_contracts::{
	banking::v1 as bk,
	book::{kind as wire_kind, resolution as wire_resolution, side as wire_side, tif as wire_tif},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
	dto,
	error::ApiError,
	routes::{editable, first_party_request, money_gate, money_token_for, parse_json_body, require_money_token, required},
	state::AppState,
};

/// `?service=&depth=` — the book's identity and how many levels a side to render.
#[derive(Deserialize)]
pub struct BookQuery {
	pub service: Option<String>,
	pub depth: Option<u32>,
}

/// `?service=&limit=` — a tape or a history page. `service` is optional on the caller's
/// own lists (absent = every allocation) and required on the public tape.
#[derive(Deserialize)]
pub struct PageQuery {
	service: Option<String>,
	limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct ServiceQuery {
	service: Option<String>,
}

#[derive(Deserialize)]
pub struct CandlesQuery {
	service: Option<String>,
	resolution: Option<String>,
	from: Option<i64>,
	to: Option<i64>,
}

/// The one allocation a per-book read names. A missing or blank service is a client error
/// decided here — never a call to the hub with an empty id.
pub fn required_service(service: Option<String>) -> Result<String, ApiError> {
	service.filter(|s| !s.trim().is_empty()).ok_or_else(|| ApiError::BadRequest("service is required".into()))
}

/// A word the contract pins: present, and one of `expected`. The refusal names the
/// vocabulary, so a client that sent `BUY` or `fok` learns what would have worked.
fn vocabulary(v: &Value, key: &str, expected: &[&str]) -> Result<String, ApiError> {
	let Some(word) = required(v, key) else {
		return Err(ApiError::BadRequest(format!("{key} is required — one of {}", expected.join(", "))));
	};
	if !expected.contains(&word.as_str()) {
		return Err(ApiError::BadRequest(format!("unknown {key} '{word}' — expected one of {}", expected.join(", "))));
	}
	Ok(word)
}

// ── the book (readable by anyone who may view the allocation) ────────────────

/// `GET /api/book?service=&depth=` — the aggregated book: the top `depth` levels a side,
/// the last trade, the 24h figures and NAV for the ticker. A hidden allocation is a 404,
/// as on `/api/allocations/detail`. An absent `depth` is the hub's default.
pub async fn get_book(State(st): State<AppState>, jar: CookieJar, Query(q): Query<BookQuery>) -> Result<Json<dto::BookSnapshot>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_money_token(&st, &jar).await?;
	let book = st
		.grpc
		.get_book(&token, &service, q.depth.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "book unavailable"))?;
	Ok(Json(book.into()))
}

/// `GET /api/book/trades?service=&limit=` — the public tape, newest first: price, size,
/// the taker's side and the time. No order ids and no parties.
pub async fn list_trades(State(st): State<AppState>, jar: CookieJar, Query(q): Query<PageQuery>) -> Result<Json<dto::TradeList>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_money_token(&st, &jar).await?;
	let tape = st
		.grpc
		.list_trades(&token, &service, q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "trades unavailable"))?;
	Ok(Json(tape.into()))
}

/// `GET /api/book/candles?service=&resolution=&from=&to=` — OHLCV buckets over
/// `[from, to)` in unix seconds; an absent `to` is now. Buckets with no trade are omitted.
pub async fn list_candles(State(st): State<AppState>, jar: CookieJar, Query(q): Query<CandlesQuery>) -> Result<Json<dto::CandleList>, ApiError> {
	let service = required_service(q.service)?;
	let resolution = match q.resolution.as_deref().filter(|r| !r.is_empty()) {
		Some(r) if wire_resolution::is_known(r) => r.to_string(),
		Some(r) => return Err(ApiError::BadRequest(format!("unknown resolution '{r}' — expected one of {}", wire_resolution::ALL.join(", ")))),
		None => return Err(ApiError::BadRequest(format!("resolution is required — one of {}", wire_resolution::ALL.join(", ")))),
	};
	let token = require_money_token(&st, &jar).await?;
	let req = bk::ListCandlesRequest {
		service,
		resolution,
		from: q.from.unwrap_or(0),
		to: q.to.unwrap_or(0),
	};
	let candles = st.grpc.list_candles(&token, req).await.map_err(|s| ApiError::read(s, "candles unavailable"))?;
	Ok(Json(candles.into()))
}

/// `GET /api/book/policy?service=` — the allocation's trading terms: whether the book is
/// open, the taker fee, tick, lot and market slippage. Part of deciding whether to trade
/// at all, so not gated on holding a position.
pub async fn get_policy(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ServiceQuery>) -> Result<Json<dto::BookPolicy>, ApiError> {
	let service = required_service(q.service)?;
	let token = require_money_token(&st, &jar).await?;
	let policy = st.grpc.get_book_policy(&token, &service).await.map_err(|s| ApiError::read(s, "book policy unavailable"))?;
	Ok(Json(policy.into()))
}

// ── the caller's orders ──────────────────────────────────────────────────────

/// `POST /api/book/orders` — first-party, CSRF-checked and rate-limited: place an order. Body
/// `{service, side, kind, tif?, price?, size, client_order_id}`. `tif` may be left out on
/// a market order (always `ioc`); a limit order has to say. `price` is empty for a market
/// order. Idempotent by `client_order_id`: a retry returns the order the id already names.
pub async fn place_order(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Order>, ApiError> {
	let claims = money_gate(&st, &jar, &headers).await?;
	let v = parse_json_body(&headers, &body)?;
	let Some(service) = required(&v, "service") else {
		return Err(ApiError::BadRequest("service is required".into()));
	};
	let side = vocabulary(&v, "side", &wire_side::ALL)?;
	let kind = vocabulary(&v, "kind", &wire_kind::ALL)?;
	let tif = match required(&v, "tif") {
		Some(tif) if wire_tif::is_known(&tif) => tif,
		Some(tif) => return Err(ApiError::BadRequest(format!("unknown tif '{tif}' — expected one of {}", wire_tif::ALL.join(", ")))),
		None => String::new(),
	};
	let (Some(size), Some(client_order_id)) = (required(&v, "size"), required(&v, "client_order_id")) else {
		return Err(ApiError::BadRequest("size and client_order_id are required".into()));
	};
	let token = money_token_for(&st, &claims).await?;
	let req = bk::PlaceOrderRequest {
		service,
		side,
		kind,
		tif,
		price: editable(&v, "price"),
		size,
		client_order_id,
	};
	Ok(Json(st.grpc.place_order(&token, req).await?.into()))
}

/// `POST /api/book/orders/cancel` — first-party and CSRF-checked: take the caller's resting order off the
/// book; its unspent escrow is released. Never refused for a frozen caller.
pub async fn cancel_order(State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, body: Bytes) -> Result<Json<dto::Order>, ApiError> {
	// Taking an order off the book only releases the caller's own escrow, so it is first-party
	// and CSRF-checked but kept out of the per-user budget.
	first_party_request(&st, &jar, &headers)?;
	let Some(order_id) = required(&parse_json_body(&headers, &body)?, "order_id") else {
		return Err(ApiError::BadRequest("order_id is required".into()));
	};
	let token = require_money_token(&st, &jar).await?;
	Ok(Json(st.grpc.cancel_order(&token, &order_id).await?.into()))
}

/// `GET /api/book/orders?service=` — the caller's resting orders, oldest first. An absent
/// `service` is every allocation.
pub async fn list_open_orders(State(st): State<AppState>, jar: CookieJar, Query(q): Query<ServiceQuery>) -> Result<Json<dto::OrderList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let orders = st
		.grpc
		.list_open_orders(&token, &q.service.unwrap_or_default())
		.await
		.map_err(|s| ApiError::read(s, "orders unavailable"))?;
	Ok(Json(orders.into()))
}

/// `GET /api/book/orders/history?service=&limit=` — the caller's orders in every state,
/// newest first. An absent `service` is every allocation; an absent `limit` the hub's page.
pub async fn list_order_history(State(st): State<AppState>, jar: CookieJar, Query(q): Query<PageQuery>) -> Result<Json<dto::OrderList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let orders = st
		.grpc
		.list_order_history(&token, &q.service.unwrap_or_default(), q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "order history unavailable"))?;
	Ok(Json(orders.into()))
}

/// `GET /api/book/fills?service=&limit=` — the caller's own fills, newest first, each with
/// their side, their order and the fee they paid as taker.
pub async fn list_fills(State(st): State<AppState>, jar: CookieJar, Query(q): Query<PageQuery>) -> Result<Json<dto::TradeList>, ApiError> {
	let token = require_money_token(&st, &jar).await?;
	let fills = st
		.grpc
		.list_user_trades(&token, &q.service.unwrap_or_default(), q.limit.unwrap_or(0))
		.await
		.map_err(|s| ApiError::read(s, "fills unavailable"))?;
	Ok(Json(fills.into()))
}

#[cfg(test)]
mod book_route_tests {
	// `Status` is a large error type tonic mandates in handler signatures.
	#![allow(clippy::result_large_err)]

	use std::{
		net::SocketAddr,
		pin::Pin,
		sync::{Arc, Mutex},
	};

	use axum::{
		Router,
		body::Body,
		http::{Request, StatusCode, header},
	};
	use evbanking_contracts::banking::v1::{
		auth_service_server::{AuthService as BkAuthService, AuthServiceServer as BkAuthServiceServer},
		book_service_server::{BookService, BookServiceServer},
	};
	use evconcierge_auth::{TokenType, Verifier, VerifierConfig};
	use evconcierge_contracts::concierge::v1::{
		self as cc,
		auth_service_server::AuthServiceServer as CcAuthServiceServer,
		user_directory_server::{UserDirectory, UserDirectoryServer},
	};
	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::{TcpListener, TcpStream},
	};
	use tokio_stream::{Stream, wrappers::TcpListenerStream};
	use tonic::{Code, Request as GrpcRequest, Response as GrpcResponse, Status, transport::Server};
	use tower::ServiceExt;

	use super::*;
	use crate::{
		config::AppConfig,
		cookies::CookieNames,
		routes::{
			router,
			test_support::{AUDIENCE, ConciergeJwks, ISSUER, access_token},
		},
		session::BankingTokens,
		state::{AppState, Grpc},
		util::now_secs,
	};

	const CSRF: &str = "csrf-token-value";
	const SERVICE: &str = "quy-nhon";
	const ORDER_ID: &str = "3f1c1a52-4b7e-4d0a-9d6c-0f5e2b7a9c11";
	/// The one browser origin the socket accepts, as production configures it.
	const WS_ORIGIN: &str = "https://cabinet.test";
	/// What the stub's exchange seam mints. The book RPCs accept nothing else.
	const MONEY_TOKEN: &str = "banking-access-token";

	// ── the stub hub ────────────────────────────────────────────────────────────

	/// What the BFF actually forwarded upstream, so a test can assert on the request the
	/// hub would have seen rather than only on the response the browser gets.
	#[derive(Default)]
	struct Seen {
		place: Option<bk::PlaceOrderRequest>,
		cancel: Option<bk::CancelOrderRequest>,
		open_orders: Option<bk::ListOpenOrdersRequest>,
		history: Option<bk::ListOrderHistoryRequest>,
		user_trades: Option<bk::ListUserTradesRequest>,
		book: Option<bk::GetBookRequest>,
		trades: Option<bk::ListTradesRequest>,
		candles: Option<bk::ListCandlesRequest>,
		watch: Option<bk::WatchBookRequest>,
		set_policy: Option<bk::SetBookPolicyRequest>,
		money_tokens_issued: usize,
	}

	#[derive(Clone)]
	struct Hub {
		role: String,
		/// What `GetMe` resolves the seat to — what `require_permission` gates the policy write on.
		permissions: Vec<String>,
		/// When set, every book RPC fails with this code (the upstream-refusal cases).
		fail_with: Option<Code>,
		seen: Arc<Mutex<Seen>>,
	}

	impl Hub {
		fn new(role: &str) -> Self {
			Self {
				role: role.to_string(),
				permissions: concierge_domain::authz::Role::parse(role)
					.expect("stubs sit in concierge's seats")
					.permissions()
					.iter()
					.map(|p| (*p).to_owned())
					.collect(),
				fail_with: None,
				seen: Arc::new(Mutex::new(Seen::default())),
			}
		}

		fn failing(role: &str, code: Code) -> Self {
			Self {
				fail_with: Some(code),
				..Self::new(role)
			}
		}

		/// The book is a MONEY plane: it must be reached with the banking token minted
		/// through the exchange seam, never with the concierge identity JWT from the cookie.
		/// A stub that accepted any bearer would let a handler swapping `require_money_token`
		/// for `require_token` pass every test here and fail only in prod.
		fn guard_money_plane<T>(&self, request: &GrpcRequest<T>) -> Result<(), Status> {
			let presented = request.metadata().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
			if presented != format!("Bearer {MONEY_TOKEN}") {
				return Err(Status::unauthenticated("the book requires the banking money token, not the concierge identity token"));
			}
			match self.fail_with {
				Some(code) => Err(Status::new(code, "upstream refused")),
				None => Ok(()),
			}
		}
	}

	fn stub_order(id: &str, state: &str) -> bk::Order {
		bk::Order {
			id: id.into(),
			service: SERVICE.into(),
			user_id: "bank-user-1".into(),
			side: "buy".into(),
			kind: "limit".into(),
			tif: "gtc".into(),
			price: "101.50".into(),
			size: "10.0000".into(),
			filled: "2.5000".into(),
			remaining: "7.5000".into(),
			avg_fill_price: "101.25".into(),
			fee_paid: "0.25".into(),
			state: state.into(),
			reject_reason: String::new(),
			cancel_reason: if state == "cancelled" { "user".into() } else { String::new() },
			client_order_id: "cli-1".into(),
			created_at: 1_750_000_000,
			updated_at: 1_750_000_100,
		}
	}

	fn stub_trade(own: bool) -> bk::Trade {
		bk::Trade {
			id: "trade-1".into(),
			service: SERVICE.into(),
			price: "101.25".into(),
			size: "2.5000".into(),
			taker_side: "sell".into(),
			executed_at: 1_750_000_100,
			user_side: if own { "buy".into() } else { String::new() },
			order_id: if own { ORDER_ID.into() } else { String::new() },
			fee: if own { "0.25".into() } else { String::new() },
		}
	}

	fn stub_snapshot(depth: u32) -> bk::BookSnapshot {
		bk::BookSnapshot {
			service: SERVICE.into(),
			revision: 42,
			bids: (0..depth.min(2))
				.map(|i| bk::BookLevel {
					price: format!("{}.00", 100 - i),
					size: "5.0000".into(),
					orders: 2,
				})
				.collect(),
			asks: vec![bk::BookLevel {
				price: "102.00".into(),
				size: "1.0000".into(),
				orders: 1,
			}],
			last_price: "101.25".into(),
			last_side: "sell".into(),
			mid: "101.00".into(),
			spread: "2.00".into(),
			nav: "99.80".into(),
			volume_24h: "12.5000".into(),
			change_24h: "-2.5".into(),
			as_of: 1_750_000_200,
		}
	}

	fn stub_policy(req: Option<&bk::SetBookPolicyRequest>) -> bk::BookPolicy {
		match req {
			Some(r) => bk::BookPolicy {
				service: r.service.clone(),
				book_open: r.book_open,
				taker_fee_bps: r.taker_fee_bps,
				price_tick: if r.price_tick.is_empty() { "0.01".into() } else { r.price_tick.clone() },
				lot_size: if r.lot_size.is_empty() { "0.0001".into() } else { r.lot_size.clone() },
				market_slippage_bps: r.market_slippage_bps,
				allow_unbacked_trading: r.allow_unbacked_trading,
				updated_at: 1_750_000_300,
			},
			None => bk::BookPolicy {
				service: SERVICE.into(),
				book_open: true,
				taker_fee_bps: 25,
				price_tick: "0.01".into(),
				lot_size: "0.0001".into(),
				market_slippage_bps: 100,
				allow_unbacked_trading: true,
				updated_at: 1_750_000_300,
			},
		}
	}

	#[tonic::async_trait]
	impl UserDirectory for Hub {
		/// `require_permission` reads the caller's permissions from here per request — the
		/// only thing standing between an investor and the policy write.
		async fn get_me(&self, _: GrpcRequest<cc::GetMeRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
			Ok(GrpcResponse::new(cc::UserProfile {
				user_id: "user-1".into(),
				role: self.role.clone(),
				permissions: self.permissions.clone(),
				..Default::default()
			}))
		}

		async fn update_profile(&self, _: GrpcRequest<cc::UpdateProfileRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn revoke_tokens(&self, _: GrpcRequest<cc::RevokeTokensRequest>) -> Result<GrpcResponse<cc::RevokeTokensResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn disable_user(&self, _: GrpcRequest<cc::DisableUserRequest>) -> Result<GrpcResponse<cc::DisableUserResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn hold_user(&self, _: GrpcRequest<cc::HoldUserRequest>) -> Result<GrpcResponse<cc::HoldUserResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn reinstate_user(&self, _: GrpcRequest<cc::ReinstateUserRequest>) -> Result<GrpcResponse<cc::ReinstateUserResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn set_kyc_level(&self, _: GrpcRequest<cc::SetKycLevelRequest>) -> Result<GrpcResponse<cc::SetKycLevelResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn list_users(&self, _: GrpcRequest<cc::ListUsersRequest>) -> Result<GrpcResponse<cc::ListUsersResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn get_user(&self, _: GrpcRequest<cc::GetUserRequest>) -> Result<GrpcResponse<cc::UserProfile>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn set_role(&self, _: GrpcRequest<cc::SetRoleRequest>) -> Result<GrpcResponse<cc::SetRoleResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn grant_permission(&self, _: GrpcRequest<cc::GrantPermissionRequest>) -> Result<GrpcResponse<cc::GrantPermissionResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn revoke_permission(&self, _: GrpcRequest<cc::RevokePermissionRequest>) -> Result<GrpcResponse<cc::RevokePermissionResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn list_grants(&self, _: GrpcRequest<cc::ListGrantsRequest>) -> Result<GrpcResponse<cc::ListGrantsResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}
	}

	#[tonic::async_trait]
	impl BkAuthService for Hub {
		/// The concierge→banking exchange seam. Every book route needs a money token, so
		/// counting the mints is also how these tests prove a rejected request never got far
		/// enough to ask for one.
		async fn issue_user_token(&self, _: GrpcRequest<bk::IssueUserTokenRequest>) -> Result<GrpcResponse<bk::TokenResponse>, Status> {
			self.seen.lock().unwrap().money_tokens_issued += 1;
			Ok(GrpcResponse::new(bk::TokenResponse {
				access_token: MONEY_TOKEN.into(),
				access_expires_at: now_secs() + 900,
				refresh_token: "banking-refresh-token".into(),
				refresh_expires_at: now_secs() + 86_400,
				user: None,
			}))
		}

		async fn refresh(&self, _: GrpcRequest<bk::RefreshRequest>) -> Result<GrpcResponse<bk::TokenResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn logout(&self, _: GrpcRequest<bk::LogoutRequest>) -> Result<GrpcResponse<bk::LogoutResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn list_sessions(&self, _: GrpcRequest<bk::ListSessionsRequest>) -> Result<GrpcResponse<bk::ListSessionsResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn revoke_session(&self, _: GrpcRequest<bk::RevokeSessionRequest>) -> Result<GrpcResponse<bk::RevokeSessionResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}

		async fn jwks(&self, _: GrpcRequest<bk::JwksRequest>) -> Result<GrpcResponse<bk::JwksResponse>, Status> {
			Err(Status::unimplemented("not reached by the book routes"))
		}
	}

	#[tonic::async_trait]
	impl BookService for Hub {
		type WatchBookStream = Pin<Box<dyn Stream<Item = Result<bk::BookEvent, Status>> + Send + 'static>>;

		async fn place_order(&self, request: GrpcRequest<bk::PlaceOrderRequest>) -> Result<GrpcResponse<bk::Order>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().place = Some(req.clone());
			Ok(GrpcResponse::new(bk::Order {
				side: req.side,
				kind: req.kind,
				tif: req.tif,
				size: req.size,
				client_order_id: req.client_order_id,
				..stub_order(ORDER_ID, "open")
			}))
		}

		async fn cancel_order(&self, request: GrpcRequest<bk::CancelOrderRequest>) -> Result<GrpcResponse<bk::Order>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().cancel = Some(req.clone());
			Ok(GrpcResponse::new(stub_order(&req.order_id, "cancelled")))
		}

		async fn list_open_orders(&self, request: GrpcRequest<bk::ListOpenOrdersRequest>) -> Result<GrpcResponse<bk::OrderList>, Status> {
			self.guard_money_plane(&request)?;
			self.seen.lock().unwrap().open_orders = Some(request.into_inner());
			Ok(GrpcResponse::new(bk::OrderList {
				orders: vec![stub_order(ORDER_ID, "partially_filled")],
			}))
		}

		async fn list_order_history(&self, request: GrpcRequest<bk::ListOrderHistoryRequest>) -> Result<GrpcResponse<bk::OrderList>, Status> {
			self.guard_money_plane(&request)?;
			self.seen.lock().unwrap().history = Some(request.into_inner());
			Ok(GrpcResponse::new(bk::OrderList {
				orders: vec![stub_order(ORDER_ID, "filled")],
			}))
		}

		async fn list_user_trades(&self, request: GrpcRequest<bk::ListUserTradesRequest>) -> Result<GrpcResponse<bk::TradeList>, Status> {
			self.guard_money_plane(&request)?;
			self.seen.lock().unwrap().user_trades = Some(request.into_inner());
			Ok(GrpcResponse::new(bk::TradeList { trades: vec![stub_trade(true)] }))
		}

		async fn get_book(&self, request: GrpcRequest<bk::GetBookRequest>) -> Result<GrpcResponse<bk::BookSnapshot>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().book = Some(req.clone());
			Ok(GrpcResponse::new(stub_snapshot(if req.depth == 0 { 20 } else { req.depth })))
		}

		async fn list_trades(&self, request: GrpcRequest<bk::ListTradesRequest>) -> Result<GrpcResponse<bk::TradeList>, Status> {
			self.guard_money_plane(&request)?;
			self.seen.lock().unwrap().trades = Some(request.into_inner());
			Ok(GrpcResponse::new(bk::TradeList { trades: vec![stub_trade(false)] }))
		}

		async fn list_candles(&self, request: GrpcRequest<bk::ListCandlesRequest>) -> Result<GrpcResponse<bk::CandleList>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().candles = Some(req.clone());
			Ok(GrpcResponse::new(bk::CandleList {
				service: req.service,
				resolution: req.resolution,
				candles: vec![bk::Candle {
					time: 1_750_000_000,
					open: "100.00".into(),
					high: "102.00".into(),
					low: "99.50".into(),
					close: "101.25".into(),
					volume: "12.5000".into(),
				}],
			}))
		}

		async fn watch_book(&self, request: GrpcRequest<bk::WatchBookRequest>) -> Result<GrpcResponse<Self::WatchBookStream>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().watch = Some(req.clone());
			let frame = bk::BookEvent {
				snapshot: Some(stub_snapshot(req.depth)),
				trades: vec![stub_trade(false)],
				orders_revision: 7,
			};
			Ok(GrpcResponse::new(Box::pin(tokio_stream::iter(vec![Ok(frame)]))))
		}

		async fn get_book_policy(&self, request: GrpcRequest<bk::GetBookPolicyRequest>) -> Result<GrpcResponse<bk::BookPolicy>, Status> {
			self.guard_money_plane(&request)?;
			Ok(GrpcResponse::new(stub_policy(None)))
		}

		async fn set_book_policy(&self, request: GrpcRequest<bk::SetBookPolicyRequest>) -> Result<GrpcResponse<bk::BookPolicy>, Status> {
			self.guard_money_plane(&request)?;
			let req = request.into_inner();
			self.seen.lock().unwrap().set_policy = Some(req.clone());
			Ok(GrpcResponse::new(stub_policy(Some(&req))))
		}
	}

	// ── harness ─────────────────────────────────────────────────────────────────

	/// Serve the stub on an ephemeral port, handing tonic the listener we already hold —
	/// never re-binding, so a sibling test running in parallel cannot be handed this port
	/// and end up talking to the wrong hub.
	async fn serve(hub: Hub) -> SocketAddr {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
		let addr = listener.local_addr().expect("the listener has an address");

		tokio::spawn(async move {
			Server::builder()
				.add_service(CcAuthServiceServer::new(ConciergeJwks))
				.add_service(UserDirectoryServer::new(hub.clone()))
				.add_service(BkAuthServiceServer::new(hub.clone()))
				.add_service(BookServiceServer::new(hub))
				.serve_with_incoming(TcpListenerStream::new(listener))
				.await
				.expect("the stub hub serves");
		});

		addr
	}

	/// The real router over a real `AppState`, with all three upstream channels pointed at
	/// the one stub. Cookies take their insecure (unprefixed) names, as in development;
	/// the socket's origin guard is configured, as in production.
	fn app(addr: SocketAddr) -> Router {
		let env = |var: &str| -> Option<String> {
			Some(match var {
				"PIGGYBANK_GRPC_ADDR" | "BANKING_AUTH_GRPC_ADDR" | "CONCIERGE_GRPC_ADDR" => format!("http://{addr}"),
				"BANKING_ISSUANCE_TOKEN" => "test-issuance".into(),
				"AUTH_ISSUER" => ISSUER.into(),
				"AUTH_CLIENT_AUDIENCE" => AUDIENCE.into(),
				"MFE_REGISTRY_PATH" => "/mfe-registry.json".into(),
				"CABINET_WS_ORIGIN" => WS_ORIGIN.into(),
				"APP_ENV" => "development".into(),
				_ => return None,
			})
		};
		let config = AppConfig::from_source(env).expect("the test env loads");
		let verifier = Verifier::try_new(VerifierConfig {
			issuer: ISSUER.into(),
			audiences: vec![AUDIENCE.into()],
			allowed_types: vec![TokenType::Access],
			jwks_grpc_endpoint: format!("http://{addr}"),
		})
		.expect("build the verifier");
		let endpoint = format!("http://{addr}");

		router(AppState {
			cookies: Arc::new(CookieNames::new(config.cookie_secure())),
			banking: Arc::new(BankingTokens::new()),
			approvals: Arc::new(crate::routes::approval::AttemptLimiter::default()),
			money_mutations: Arc::new(crate::routes::money_mutation_limiter()),
			wallet_actions: None,
			verifier,
			grpc: Grpc::connect_lazy(&endpoint, &endpoint, &endpoint, Some("test-issuance".into())).expect("build the lazy channels"),
			config: Arc::new(config),
		})
	}

	/// A request carrying the signed session cookie (and, for mutations, the matching
	/// CSRF pair and the page's own `Origin`) — what the browser actually sends.
	fn signed(method: &str, uri: &str, body: Option<&str>, csrf: bool) -> Request<Body> {
		let cookie = format!("ev_access={}; ev_csrf={CSRF}", access_token());
		let mut builder = Request::builder().method(method).uri(uri).header(header::COOKIE, cookie);
		if csrf {
			builder = builder.header("x-ev-csrf", CSRF).header(header::ORIGIN, WS_ORIGIN);
		}
		match body {
			Some(json) => builder.header(header::CONTENT_TYPE, "application/json").body(Body::from(json.to_owned())),
			None => builder.body(Body::empty()),
		}
		.expect("build the request")
	}

	/// The router on a real listener. A websocket upgrade needs hyper's upgrade machinery,
	/// which `oneshot` does not supply — without it the extractor answers 426 before the
	/// handler's own gates run — so the socket tests go over TCP like a browser would.
	async fn listen(app: Router) -> SocketAddr {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("claim an ephemeral port");
		let addr = listener.local_addr().expect("the listener has an address");
		tokio::spawn(async move {
			axum::serve(listener, app).await.expect("the BFF serves");
		});
		addr
	}

	/// A websocket handshake as a browser sends it: the upgrade headers, the cookie when
	/// signed in, and the page's `Origin` unless a cross-site page is being simulated.
	/// Answers the status the handshake got, and the connection — upgraded on a 101, with
	/// the first frame next on the wire.
	async fn handshake(addr: SocketAddr, uri: &str, signed_in: bool, origin: Option<&str>) -> (u16, TcpStream) {
		let mut stream = TcpStream::connect(addr).await.expect("connect to the BFF");
		let mut request =
			format!("GET {uri} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n");
		if signed_in {
			request.push_str(&format!("Cookie: ev_access={}\r\n", access_token()));
		}
		if let Some(origin) = origin {
			request.push_str(&format!("Origin: {origin}\r\n"));
		}
		request.push_str("\r\n");
		stream.write_all(request.as_bytes()).await.expect("send the handshake");

		// Byte by byte up to the end of the head, so nothing past it — the first frame, on
		// a 101 — is swallowed into a read buffer.
		let mut head = Vec::new();
		let mut byte = [0u8; 1];
		while !head.ends_with(b"\r\n\r\n") {
			assert_ne!(stream.read(&mut byte).await.expect("read the response"), 0, "the BFF hung up mid-head");
			head.push(byte[0]);
		}
		let status_line = String::from_utf8_lossy(&head);
		let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status line");
		(status, stream)
	}

	/// The next frame off an upgraded connection: its opcode and payload. Server frames are
	/// unmasked, and a stub payload never reaches the eight-byte length form.
	async fn read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
		let mut head = [0u8; 2];
		stream.read_exact(&mut head).await.expect("read a frame head");
		let len = match head[1] & 0x7f {
			126 => {
				let mut ext = [0u8; 2];
				stream.read_exact(&mut ext).await.expect("read the extended length");
				usize::from(u16::from_be_bytes(ext))
			}
			127 => panic!("a stub frame is never that large"),
			n => usize::from(n),
		};
		let mut payload = vec![0u8; len];
		stream.read_exact(&mut payload).await.expect("read the payload");
		(head[0] & 0x0f, payload)
	}

	const OPCODE_TEXT: u8 = 0x1;
	const OPCODE_CLOSE: u8 = 0x8;

	async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
		let response = app.clone().oneshot(request).await.expect("the router responds");
		let status = response.status();
		let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("read the body");
		(status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
	}

	const PLACE: &str = r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"gtc","price":"101.50","size":"10","client_order_id":"cli-1"}"#;

	// ── the gates ───────────────────────────────────────────────────────────────

	/// Every book route is behind the session cookie. Without one nothing is read, nothing
	/// is written, and no money-plane token is ever minted for the caller.
	#[tokio::test]
	async fn no_session_reaches_nothing() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for (method, uri, body) in [
			("GET", "/api/book?service=quy-nhon", None),
			("GET", "/api/book/trades?service=quy-nhon", None),
			("GET", "/api/book/candles?service=quy-nhon&resolution=1h", None),
			("GET", "/api/book/policy?service=quy-nhon", None),
			("GET", "/api/book/orders", None),
			("GET", "/api/book/orders/history", None),
			("GET", "/api/book/fills", None),
			("POST", "/api/book/orders", Some(PLACE)),
			("POST", "/api/book/orders/cancel", Some(r#"{"order_id":"x"}"#)),
			("POST", "/api/admin/allocations/book", Some(r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25}"#)),
		] {
			// The CSRF pair and the page's origin are present so the mutations get past the
			// first-party gate (which runs first) and are refused on the SESSION, the thing
			// this test is about.
			let mut builder = Request::builder()
				.method(method)
				.uri(uri)
				.header(header::COOKIE, format!("ev_csrf={CSRF}"))
				.header("x-ev-csrf", CSRF)
				.header(header::ORIGIN, WS_ORIGIN);
			if body.is_some() {
				builder = builder.header(header::CONTENT_TYPE, "application/json");
			}
			let request = builder.body(body.map_or_else(Body::empty, |b: &str| Body::from(b.to_owned()))).unwrap();
			let (status, _) = send(&app, request).await;
			assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} must refuse an unauthenticated caller");
		}
		let addr = listen(app).await;
		let (status, _) = handshake(addr, "/api/book/ws?service=quy-nhon", false, Some(WS_ORIGIN)).await;
		assert_eq!(status, 401, "the socket must refuse an unauthenticated handshake");

		let seen = seen.lock().unwrap();
		assert_eq!(seen.money_tokens_issued, 0, "an unauthenticated request must never mint a money-plane token");
		assert!(seen.place.is_none() && seen.cancel.is_none() && seen.set_policy.is_none() && seen.watch.is_none());
	}

	/// The double-submit gate on the mutations. A valid session is not enough: a cross-site
	/// post carries the cookie but cannot read it to echo the header.
	#[tokio::test]
	async fn a_mutation_without_the_csrf_header_is_refused() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, _) = send(&app, signed("POST", "/api/book/orders", Some(PLACE), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "placing must require the CSRF echo");
		let (status, _) = send(&app, signed("POST", "/api/book/orders/cancel", Some(r#"{"order_id":"x"}"#), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "cancelling must require the CSRF echo");
		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25}"#;
		let (status, _) = send(&app, signed("POST", "/api/admin/allocations/book", Some(body), false)).await;
		assert_eq!(status, StatusCode::FORBIDDEN, "setting the policy must require the CSRF echo");

		let seen = seen.lock().unwrap();
		assert!(
			seen.place.is_none() && seen.cancel.is_none() && seen.set_policy.is_none(),
			"a CSRF failure must be decided before the hub is called"
		);
		assert_eq!(seen.money_tokens_issued, 0, "a request refused on CSRF must not cost a money-token mint");
	}

	/// Trading moves money, so the order routes are first-party like the wallet's: a valid
	/// session and a correct CSRF pair from any other page — a scope's host, a look-alike, a
	/// fetch the browser marks cross-site — are refused before the hub or a mint is reached.
	#[tokio::test]
	async fn the_order_routes_are_first_party_only() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for (uri, body) in [("/api/book/orders", PLACE), ("/api/book/orders/cancel", r#"{"order_id":"x"}"#)] {
			for origin in ["https://sa.evinvest.ltd", "https://evil.test", "https://cabinet.test.evil.test", "https://cabinet.test/", "null"] {
				let mut request = signed("POST", uri, Some(body), true);
				request.headers_mut().insert(header::ORIGIN, origin.parse().unwrap());
				let (status, _) = send(&app, request).await;
				assert_eq!(status, StatusCode::FORBIDDEN, "{uri} from {origin}");
			}
			let mut cross_site = signed("POST", uri, Some(body), true);
			cross_site.headers_mut().insert("sec-fetch-site", "cross-site".parse().unwrap());
			let (status, _) = send(&app, cross_site).await;
			assert_eq!(status, StatusCode::FORBIDDEN, "{uri} marked cross-site");

			let mut plain = signed("POST", uri, Some(body), true);
			plain.headers_mut().insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
			let (status, _) = send(&app, plain).await;
			assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{uri} as text/plain");
		}
		let seen = seen.lock().unwrap();
		assert!(seen.place.is_none() && seen.cancel.is_none(), "a refused order must never reach the hub");
	}

	/// The socket's half of CSRF: a handshake from any origin but the cabinet's is refused,
	/// with the cookie present and valid — before the feed is subscribed.
	#[tokio::test]
	async fn a_cross_site_handshake_is_refused() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let addr = listen(app(serve(hub).await)).await;

		for origin in [None, Some("https://evil.test")] {
			let (status, _) = handshake(addr, "/api/book/ws?service=quy-nhon", true, origin).await;
			assert_eq!(status, 403, "origin {origin:?} must be refused");
		}
		let seen = seen.lock().unwrap();
		assert!(seen.watch.is_none(), "a refused handshake must never subscribe upstream");
		assert_eq!(seen.money_tokens_issued, 0, "a refused handshake must not cost a money-token mint");
	}

	/// The policy write is the operator's alone. A plain investor holds a perfectly valid
	/// session — the role is what stops them, read from the directory per request.
	#[tokio::test]
	async fn an_investor_cannot_set_the_policy() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25}"#;
		let (status, _) = send(&app, signed("POST", "/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::FORBIDDEN);
		assert!(seen.lock().unwrap().set_policy.is_none(), "a refused caller must never reach the hub");
	}

	// ── reading ─────────────────────────────────────────────────────────────────

	/// The snapshot the screen opens on. `revision` (uint64) and `as_of` (int64) cross as
	/// STRINGS, the levels keep their side order, and `depth` reaches the hub as asked.
	#[tokio::test]
	async fn the_book_reaches_the_browser_intact() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(&app, signed("GET", "/api/book?service=quy-nhon&depth=5", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["service"], SERVICE);
		assert_eq!(body["revision"], "42", "revision must serialize as a string, not a number");
		assert_eq!(body["bids"][0]["price"], "100.00");
		assert_eq!(body["bids"][1]["price"], "99.00", "bids are best (highest) first");
		assert_eq!(body["bids"][0]["orders"], 2);
		assert_eq!(body["asks"][0]["price"], "102.00");
		assert_eq!(body["last_price"], "101.25");
		assert_eq!(body["last_side"], "sell");
		assert_eq!(body["mid"], "101.00");
		assert_eq!(body["spread"], "2.00");
		assert_eq!(body["nav"], "99.80");
		assert_eq!(body["volume_24h"], "12.5000");
		assert_eq!(body["change_24h"], "-2.5");
		assert_eq!(body["as_of"], "1750000200", "as_of must serialize as a string, not a number");

		let seen = seen.lock().unwrap();
		assert_eq!(seen.book.as_ref().map(|r| r.depth), Some(5));
		assert_eq!(seen.money_tokens_issued, 1, "a served read must have crossed to the money plane");
	}

	/// The public tape discloses no party: the caller-only fields cross EMPTY, never
	/// dropped (the browser type has them) and never filled in.
	#[tokio::test]
	async fn the_public_tape_names_no_party() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(&app, signed("GET", "/api/book/trades?service=quy-nhon&limit=30", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let trade = &body["trades"][0];
		assert_eq!(trade["price"], "101.25");
		assert_eq!(trade["taker_side"], "sell");
		assert_eq!(trade["executed_at"], "1750000100");
		assert_eq!(trade["user_side"], "");
		assert_eq!(trade["order_id"], "");
		assert_eq!(trade["fee"], "");
		assert_eq!(seen.lock().unwrap().trades.as_ref().map(|r| r.limit), Some(30));
	}

	/// The caller's own tape carries their side, their order and the fee they paid.
	#[tokio::test]
	async fn own_fills_carry_the_side_the_order_and_the_fee() {
		let app = app(serve(Hub::new("investor")).await);

		let (status, body) = send(&app, signed("GET", "/api/book/fills?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let fill = &body["trades"][0];
		assert_eq!(fill["user_side"], "buy");
		assert_eq!(fill["order_id"], ORDER_ID);
		assert_eq!(fill["fee"], "0.25");
	}

	/// The three per-caller lists take an OPTIONAL allocation: absent means every one, and
	/// that is forwarded as the empty filter the hub reads that way, never refused.
	#[tokio::test]
	async fn own_lists_without_an_allocation_mean_every_allocation() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for uri in ["/api/book/orders", "/api/book/orders/history?limit=10", "/api/book/fills"] {
			let (status, _) = send(&app, signed("GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::OK, "{uri}");
		}
		let (status, body) = send(&app, signed("GET", "/api/book/orders?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		let order = &body["orders"][0];
		assert_eq!(order["state"], "partially_filled");
		assert_eq!(order["remaining"], "7.5000");
		assert_eq!(order["created_at"], "1750000000", "created_at must serialize as a string, not a number");

		let seen = seen.lock().unwrap();
		assert_eq!(seen.open_orders.as_ref().map(|r| r.service.as_str()), Some(SERVICE), "the last call named the allocation");
		assert_eq!(seen.history.as_ref().map(|r| (r.service.as_str(), r.limit)), Some(("", 10)));
		assert_eq!(seen.user_trades.as_ref().map(|r| (r.service.as_str(), r.limit)), Some(("", 0)));
	}

	/// Every per-book read names its allocation in the query string, and a missing one is a
	/// client error decided here — never a call to the hub with an empty service id.
	#[tokio::test]
	async fn a_per_book_read_without_an_allocation_is_rejected_locally() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for uri in [
			"/api/book",
			"/api/book?service=",
			"/api/book/trades?service=%20",
			"/api/book/candles?resolution=1h",
			"/api/book/policy",
		] {
			let (status, _) = send(&app, signed("GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} must be rejected before the hub is called");
		}
		let addr = listen(app).await;
		let (status, _) = handshake(addr, "/api/book/ws", true, Some(WS_ORIGIN)).await;
		assert_eq!(status, 400, "the socket names one book; without it there is nothing to watch");

		assert_eq!(seen.lock().unwrap().money_tokens_issued, 0, "a request rejected on its own input must not mint a money token");
	}

	/// Candles: the resolution is checked against the contract's words here, and the
	/// window crosses as given — an absent `to` is 0, which the hub reads as now.
	#[tokio::test]
	async fn candles_check_the_resolution_and_forward_the_window() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for uri in ["/api/book/candles?service=quy-nhon", "/api/book/candles?service=quy-nhon&resolution=2h"] {
			let (status, body) = send(&app, signed("GET", uri, None, false)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
			assert!(body["error"].as_str().unwrap().contains("1m, 5m, 15m, 1h, 4h, 1d"), "the refusal names the vocabulary");
		}

		let (status, body) = send(&app, signed("GET", "/api/book/candles?service=quy-nhon&resolution=1h&from=1750000000", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["resolution"], "1h");
		let candle = &body["candles"][0];
		assert_eq!(candle["time"], "1750000000", "time must serialize as a string, not a number");
		assert_eq!(candle["close"], "101.25");
		assert_eq!(candle["volume"], "12.5000");

		let forwarded = seen.lock().unwrap().candles.clone().expect("the hub saw the read");
		assert_eq!((forwarded.from, forwarded.to), (1_750_000_000, 0));
	}

	/// The terms an investor reads before trading.
	#[tokio::test]
	async fn the_policy_reaches_the_browser_intact() {
		let app = app(serve(Hub::new("investor")).await);

		let (status, body) = send(&app, signed("GET", "/api/book/policy?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["book_open"], true);
		assert_eq!(body["taker_fee_bps"], 25);
		assert_eq!(body["price_tick"], "0.01");
		assert_eq!(body["lot_size"], "0.0001");
		assert_eq!(body["market_slippage_bps"], 100);
		assert_eq!(body["allow_unbacked_trading"], true, "the terminal reads the acknowledgement to show its notice");
		assert_eq!(body["updated_at"], "1750000300");
	}

	/// A read that fails upstream maps the hub's code to HTTP but never relays its wording:
	/// a hidden allocation is a 404 exactly as an unknown one, with the same fixed message.
	#[tokio::test]
	async fn a_failed_read_maps_the_code_and_hides_the_detail() {
		let app = app(serve(Hub::failing("investor", Code::NotFound)).await);

		let (status, body) = send(&app, signed("GET", "/api/book?service=quy-nhon", None, false)).await;
		assert_eq!(status, StatusCode::NOT_FOUND);
		assert_eq!(body["error"], "book unavailable");
	}

	// ── writing ─────────────────────────────────────────────────────────────────

	/// Placing forwards the whole order as given — the tuple IS the order — and the
	/// hub's answer comes back with its 64-bit fields as strings.
	#[tokio::test]
	async fn placing_forwards_the_whole_order() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let (status, body) = send(&app, signed("POST", "/api/book/orders", Some(PLACE), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["id"], ORDER_ID);
		assert_eq!(body["state"], "open");
		assert_eq!(body["client_order_id"], "cli-1");
		assert_eq!(body["updated_at"], "1750000100");
		assert_eq!(
			(body["reject_reason"].as_str(), body["cancel_reason"].as_str()),
			(Some(""), Some("")),
			"a resting order carries neither reason"
		);

		let forwarded = seen.lock().unwrap().place.clone().expect("the hub saw the order");
		assert_eq!(forwarded.service, SERVICE);
		assert_eq!((forwarded.side.as_str(), forwarded.kind.as_str(), forwarded.tif.as_str()), ("buy", "limit", "gtc"));
		assert_eq!((forwarded.price.as_str(), forwarded.size.as_str()), ("101.50", "10"));
		assert_eq!(forwarded.client_order_id, "cli-1");
	}

	/// A market order may leave `tif` and `price` out: both cross EMPTY and the hub fills
	/// in `ioc` and the derived limit. The BFF must not invent either.
	#[tokio::test]
	async fn a_market_order_leaves_tif_and_price_to_the_hub() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","side":"sell","kind":"market","size":"3","client_order_id":"cli-2"}"#;
		let (status, _) = send(&app, signed("POST", "/api/book/orders", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);

		let forwarded = seen.lock().unwrap().place.clone().expect("the hub saw the order");
		assert_eq!((forwarded.kind.as_str(), forwarded.tif.as_str(), forwarded.price.as_str()), ("market", "", ""));
	}

	/// A word outside the contract's vocabulary, or a missing required field, is refused
	/// here with the words that would have worked — before a money token is minted.
	#[tokio::test]
	async fn a_malformed_order_is_refused_before_it_reaches_the_hub() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for (body, expect) in [
			(r#"{"side":"buy","kind":"limit","tif":"gtc","price":"1","size":"1","client_order_id":"c"}"#, "service"),
			(
				r#"{"service":"quy-nhon","side":"BUY","kind":"limit","tif":"gtc","price":"1","size":"1","client_order_id":"c"}"#,
				"buy, sell",
			),
			(
				r#"{"service":"quy-nhon","side":"buy","kind":"stop","tif":"gtc","price":"1","size":"1","client_order_id":"c"}"#,
				"limit, market",
			),
			(
				r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"fok","price":"1","size":"1","client_order_id":"c"}"#,
				"gtc, ioc, alo",
			),
			(r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"gtc","price":"1","client_order_id":"c"}"#, "size"),
			(r#"{"service":"quy-nhon","side":"buy","kind":"limit","tif":"gtc","price":"1","size":"1"}"#, "client_order_id"),
		] {
			let (status, response) = send(&app, signed("POST", "/api/book/orders", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
			assert!(response["error"].as_str().unwrap().contains(expect), "{body}: {}", response["error"]);
		}
		let (status, _) = send(&app, signed("POST", "/api/book/orders/cancel", Some("{}"), true)).await;
		assert_eq!(status, StatusCode::BAD_REQUEST, "a cancel names its order");

		let seen = seen.lock().unwrap();
		assert!(seen.place.is_none() && seen.cancel.is_none(), "a malformed order must never reach the hub");
		assert_eq!(seen.money_tokens_issued, 0, "a body we are going to refuse must not cost a money-token mint");
	}

	/// A cancel forwards the id and answers with the order in its new state.
	#[tokio::test]
	async fn cancelling_forwards_the_order_id() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = format!(r#"{{"order_id":"{ORDER_ID}"}}"#);
		let (status, response) = send(&app, signed("POST", "/api/book/orders/cancel", Some(&body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["id"], ORDER_ID);
		assert_eq!(response["state"], "cancelled");
		assert_eq!(response["cancel_reason"], "user", "the hub's reason crosses verbatim");
		assert_eq!(seen.lock().unwrap().cancel.as_ref().map(|r| r.order_id.as_str()), Some(ORDER_ID));
	}

	/// A refusal on a mutation relays the hub's client-safe wording under the mapped
	/// status: a closed book is a 412 the investor can read, not a fault.
	#[tokio::test]
	async fn a_refused_mutation_relays_the_hub_wording() {
		let app = app(serve(Hub::failing("investor", Code::FailedPrecondition)).await);

		let (status, body) = send(&app, signed("POST", "/api/book/orders", Some(PLACE), true)).await;
		assert_eq!(status, StatusCode::PRECONDITION_FAILED);
		assert_eq!(body["error"], "upstream refused");
	}

	/// The operator's write forwards every term as given; an omitted tick or lot crosses
	/// empty (the hub's defaults), an omitted slippage as zero.
	#[tokio::test]
	async fn setting_the_policy_forwards_the_whole_schedule() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":30,"price_tick":"0.05","lot_size":"0.01","market_slippage_bps":50}"#;
		let (status, response) = send(&app, signed("POST", "/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["book_open"], true);
		assert_eq!(response["taker_fee_bps"], 30);
		assert_eq!(response["price_tick"], "0.05");
		assert_eq!(response["updated_at"], "1750000300");

		let forwarded = seen.lock().unwrap().set_policy.clone().expect("the hub saw the write");
		assert_eq!(forwarded.service, SERVICE);
		assert!(forwarded.book_open);
		assert_eq!((forwarded.taker_fee_bps, forwarded.market_slippage_bps), (30, 50));
		assert_eq!((forwarded.price_tick.as_str(), forwarded.lot_size.as_str()), ("0.05", "0.01"));

		let body = r#"{"service":"quy-nhon","book_open":false,"taker_fee_bps":0}"#;
		let (status, _) = send(&app, signed("POST", "/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK, "closing a book with the default tick and lot is a whole schedule");
		let forwarded = seen.lock().unwrap().set_policy.clone().expect("the hub saw the write");
		assert!(!forwarded.book_open);
		assert_eq!((forwarded.taker_fee_bps, forwarded.market_slippage_bps), (0, 0));
		assert_eq!((forwarded.price_tick.as_str(), forwarded.lot_size.as_str()), ("", ""));
	}

	/// The unbacked-trading acknowledgement crosses as given and comes back on the policy;
	/// left out, it crosses as `false` — the hub, not the BFF, decides whether the product
	/// needed it.
	#[tokio::test]
	async fn the_unbacked_trading_acknowledgement_is_forwarded_and_echoed() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":0,"allow_unbacked_trading":true}"#;
		let (status, response) = send(&app, signed("POST", "/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["allow_unbacked_trading"], true);
		assert!(seen.lock().unwrap().set_policy.clone().expect("the hub saw the write").allow_unbacked_trading);

		let body = r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":0}"#;
		let (status, response) = send(&app, signed("POST", "/api/admin/allocations/book", Some(body), true)).await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(response["allow_unbacked_trading"], false);
		assert!(!seen.lock().unwrap().set_policy.clone().expect("the hub saw the write").allow_unbacked_trading);
	}

	/// `book_open` and the taker fee are read as a whole: a missing or malformed one must
	/// fail loudly here rather than close a book, or price it at nothing, while the console
	/// answers "saved".
	#[tokio::test]
	async fn a_partial_policy_is_refused_before_it_reaches_the_hub() {
		let hub = Hub::new("admin");
		let seen = hub.seen.clone();
		let app = app(serve(hub).await);

		for body in [
			r#"{"book_open":true,"taker_fee_bps":25}"#,
			r#"{"service":"quy-nhon","taker_fee_bps":25}"#,
			r#"{"service":"quy-nhon","book_open":"yes","taker_fee_bps":25}"#,
			r#"{"service":"quy-nhon","book_open":true}"#,
			r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":"25"}"#,
			r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":-1}"#,
			r#"{"service":"quy-nhon","book_open":true,"taker_fee_bps":25,"market_slippage_bps":2.5}"#,
		] {
			let (status, _) = send(&app, signed("POST", "/api/admin/allocations/book", Some(body), true)).await;
			assert_eq!(status, StatusCode::BAD_REQUEST, "an incomplete policy must be refused: {body}");
		}
		assert!(seen.lock().unwrap().set_policy.is_none(), "an incomplete policy must never reach the hub");
	}

	// ── the socket ──────────────────────────────────────────────────────────────

	/// The whole bridge, end to end: a signed-in, same-origin handshake naming a book is
	/// upgraded, the subscription it opens upstream carries the money token and the depth
	/// asked for, the first frame is the book as the REST read would answer it, and when the
	/// hub's stream ends the socket is closed normally so the client reconnects.
	#[tokio::test]
	async fn a_same_origin_handshake_is_upgraded_and_streams_the_book() {
		let hub = Hub::new("investor");
		let seen = hub.seen.clone();
		let addr = listen(app(serve(hub).await)).await;

		let (status, mut socket) = handshake(addr, "/api/book/ws?service=quy-nhon&depth=10", true, Some(WS_ORIGIN)).await;
		assert_eq!(status, 101);

		let (opcode, payload) = read_frame(&mut socket).await;
		assert_eq!(opcode, OPCODE_TEXT);
		let frame: Value = serde_json::from_slice(&payload).expect("a frame is JSON");
		assert_eq!(frame["type"], "book");
		assert_eq!(frame["snapshot"]["service"], SERVICE);
		assert_eq!(frame["snapshot"]["revision"], "42", "the revision travels as a string");
		assert_eq!(frame["snapshot"]["bids"].as_array().map(Vec::len), Some(2));
		assert_eq!(frame["snapshot"]["nav"], "99.80");
		assert_eq!(frame["trades"][0]["user_side"], "", "the socket's tape names no party");
		assert_eq!(frame["orders_revision"], "7");

		let (opcode, payload) = read_frame(&mut socket).await;
		assert_eq!(opcode, OPCODE_CLOSE, "an ended upstream closes the socket");
		assert_eq!(
			u16::from_be_bytes([payload[0], payload[1]]),
			1000,
			"…normally, so the client reconnects rather than signs in again"
		);

		let seen = seen.lock().unwrap();
		let watched = seen.watch.as_ref().expect("the hub saw the subscription, with the money token");
		assert_eq!((watched.service.as_str(), watched.depth), (SERVICE, 10));
		assert_eq!(seen.money_tokens_issued, 1);
	}
}
