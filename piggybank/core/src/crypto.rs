//! The process-level rustls crypto provider, chosen once at start-up.
//!
//! The hub's dependency graph enables two rustls providers: `ring` (through tonic) and
//! `aws-lc-rs` (through reqwest). A rustls CLIENT built by tonic picks ring on its own, so the
//! outbound seams (the bridge, the signer) never noticed. A rustls SERVER does not guess: the
//! first TLS listener — the scope ledger's — panicked on its first handshake with "Could not
//! automatically determine the process-level CryptoProvider", taking the whole hub down. So
//! `main` installs one before any server or client is built, and every test that stands up
//! a TLS server calls this same function rather than installing a provider of its own, so a
//! test cannot pass on a provider production never installs.

use rustls::crypto::CryptoProvider;

/// Install `ring` as the process-level provider, unless one is installed already. Ring
/// because tonic's `tls-ring` already makes it the provider every outbound TLS seam uses.
/// Idempotent, so tests that share a process may each call it.
pub fn install_crypto_provider() {
	if CryptoProvider::get_default().is_some() {
		return;
	}
	// `Err` only means another thread installed one between the check and here — a provider
	// is in place either way, which is all this function promises.
	if rustls::crypto::ring::default_provider().install_default().is_err() {
		tracing::debug!("rustls crypto provider was installed concurrently; keeping that one");
	}
}
