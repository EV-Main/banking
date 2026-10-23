//! The `Origin` check that the first-party gate stands on (see
//! [`crate::routes::first_party_origin`]).

use axum::http::{HeaderMap, header};

/// Whether the request's `Origin` is exactly `expected`; a missing one is not.
pub fn origin_is(headers: &HeaderMap, expected: &str) -> bool {
	headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()).is_some_and(|origin| origin == expected)
}
