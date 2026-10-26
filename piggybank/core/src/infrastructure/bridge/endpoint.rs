//! The concierge endpoint every cross-plane seam dials — the lifecycle bridge, the scope
//! inbox and the approvals, which share one address and one shared secret, and so must
//! share one channel configuration: building it twice is how an `https://` bridge once
//! left a seam on a channel that never negotiated TLS.
//!
//! What the channel proves is decided here and nowhere else. The address's scheme picks
//! the transport (through [`config::bridge_transport`], the same reader the boot gate
//! uses, so what is refused and what is encrypted can never disagree on a spelling); a
//! pinned CA (`BRIDGE_TLS_CA_PEM_FILE`) is the trust anchor; the address's host is the name
//! the server's certificate must carry; and a client identity
//! (`BRIDGE_TLS_CLIENT_CERT_PEM_FILE` + `BRIDGE_TLS_CLIENT_KEY_PEM_FILE`) turns the seam
//! into mTLS the moment concierge asks for it, with no code change on this side.

use std::time::Duration;

use color_eyre::eyre::{Context, bail, eyre};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

use crate::config::{self, BridgeTransport};

/// The PEM files the bridge's TLS reads, held by path so the read happens once, here, and
/// a path that cannot be read stops the boot with the variable's name in the error.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BridgeTlsFiles {
	/// `BRIDGE_TLS_CA_PEM_FILE` — the trust anchor; certificates and nothing else.
	pub ca: Option<String>,
	/// `BRIDGE_TLS_CLIENT_CERT_PEM_FILE` — the hub's certificate, presented for mTLS.
	pub client_cert: Option<String>,
	/// `BRIDGE_TLS_CLIENT_KEY_PEM_FILE` — its private key.
	pub client_key: Option<String>,
}

impl BridgeTlsFiles {
	/// Read the three paths from the environment; an empty value counts as unset, so a
	/// Secret key left blank does not become a file named `""`.
	pub fn from_env() -> Self {
		let var = |key: &str| std::env::var(key).ok().filter(|s| !s.is_empty());
		Self {
			ca: var("BRIDGE_TLS_CA_PEM_FILE"),
			client_cert: var("BRIDGE_TLS_CLIENT_CERT_PEM_FILE"),
			client_key: var("BRIDGE_TLS_CLIENT_KEY_PEM_FILE"),
		}
	}
}

/// Build the endpoint for `CONCIERGE_BRIDGE_ADDR`.
///
/// Explicit deadlines so a half-open concierge surfaces as a bounded error instead of
/// stalling a poll. An `https://` address takes the TLS branch; cleartext is built as-is —
/// whether cleartext is *permitted* is the boot gate's call
/// ([`config::ensure_bridge_is_authenticated`]), asked before this is built, so that a
/// refusal names the policy and not a transport error.
///
/// Whether to encrypt is asked of [`config::bridge_transport`], never of the spelling. A
/// stricter second reading here (`starts_with("https://")`) leaves `HTTPS://…` classified
/// as TLS by the gate and untouched by the builder: `http::Uri` lowercases the scheme,
/// tonic then gets an https target with no TLS, every request fails
/// `HttpsUriWithoutTlsSupport` while the hub stays live and ready, and the pinned CA is
/// dropped on the floor.
pub fn bridge_endpoint(addr: &str, tls: &BridgeTlsFiles) -> color_eyre::Result<Endpoint> {
	let endpoint = Endpoint::from_shared(addr.to_string())
		.context("CONCIERGE_BRIDGE_ADDR must be a valid URL, e.g. http://127.0.0.1:55670")?
		.connect_timeout(Duration::from_secs(3))
		.timeout(Duration::from_secs(10));
	match config::bridge_transport(addr) {
		BridgeTransport::Tls => endpoint.tls_config(bridge_client_tls(addr, tls)?).context("failed to configure bridge TLS"),
		BridgeTransport::Loopback | BridgeTransport::Cleartext => Ok(endpoint),
	}
}

/// TLS for the hub's client side of the concierge seam.
///
/// The trust anchor is the pinned CA when set — the private-CA case a cluster-internal
/// concierge needs, and the thing that makes the channel prove *which* concierge
/// answered — else the public roots (which production never reaches: the boot gate
/// refuses an unpinned https address there).
///
/// The name is pinned explicitly to the address's host. tonic would take it from the URI
/// by default, but the pin is the point of this seam and a default is not a contract; it
/// is also read by [`config::bridge_host`], so an IPv6 literal arrives without its
/// brackets, which is the only spelling rustls accepts as a name.
///
/// A client identity is the hub's half of mTLS: both files or neither. One without the
/// other is a Secret half-written, and half of an identity presents nothing — the
/// handshake would then fail against a concierge that demands a client certificate, with
/// the hub live, ready and reading as configured.
fn bridge_client_tls(addr: &str, tls: &BridgeTlsFiles) -> color_eyre::Result<ClientTlsConfig> {
	let host = config::bridge_host(addr);
	if host.is_empty() {
		bail!("CONCIERGE_BRIDGE_ADDR {addr} has no host to pin the server certificate against");
	}
	let mut config = match &tls.ca {
		Some(ca_file) => {
			let ca = std::fs::read_to_string(ca_file).with_context(|| format!("failed to read BRIDGE_TLS_CA_PEM_FILE at {ca_file}"))?;
			// A file that is not certificates builds an EMPTY root store and says nothing about
			// it — see `config::check_pinned_ca_pem` for what that costs and what the check can
			// and cannot prove. The variable and the path belong in the message: the operator
			// fixing this is looking at a Secret, not at this file.
			config::check_pinned_ca_pem(&ca).map_err(|problem| eyre!("BRIDGE_TLS_CA_PEM_FILE at {ca_file} is not a trust anchor: {problem}"))?;
			ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca))
		}
		None => ClientTlsConfig::new().with_enabled_roots(),
	}
	.domain_name(host);
	match (&tls.client_cert, &tls.client_key) {
		(Some(cert_file), Some(key_file)) => {
			let cert = std::fs::read_to_string(cert_file).with_context(|| format!("failed to read BRIDGE_TLS_CLIENT_CERT_PEM_FILE at {cert_file}"))?;
			let key = std::fs::read_to_string(key_file).with_context(|| format!("failed to read BRIDGE_TLS_CLIENT_KEY_PEM_FILE at {key_file}"))?;
			config = config.identity(Identity::from_pem(cert, key));
		}
		(None, None) => {}
		(Some(_), None) => bail!("BRIDGE_TLS_CLIENT_CERT_PEM_FILE is set but BRIDGE_TLS_CLIENT_KEY_PEM_FILE is not: a client identity is both files or neither"),
		(None, Some(_)) => bail!("BRIDGE_TLS_CLIENT_KEY_PEM_FILE is set but BRIDGE_TLS_CLIENT_CERT_PEM_FILE is not: a client identity is both files or neither"),
	}
	Ok(config)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The smallest thing `config::check_pinned_ca_pem` accepts. What the body decodes to is
	/// irrelevant here — these tests prove the wiring (which addresses read which files, and
	/// that a refusal stops the boot); the gate itself is pinned against a real certificate
	/// in `config`'s own tests, and the handshake against a real CA in `tests/bridge_tls.rs`.
	const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n";

	/// Writes `contents` to a unique temp file and hands back its path; the caller unlinks it.
	fn temp_file(contents: &str) -> std::path::PathBuf {
		let path = std::env::temp_dir().join(format!("piggybank-bridge-tls-{}.pem", uuid::Uuid::new_v4()));
		std::fs::write(&path, contents).expect("the temp dir must be writable, or this test proves nothing");
		path
	}

	fn pinned(path: &std::path::Path) -> BridgeTlsFiles {
		BridgeTlsFiles {
			ca: Some(path.to_str().expect("a utf-8 temp path").to_string()),
			..BridgeTlsFiles::default()
		}
	}

	/// TLS is decided by [`config::bridge_transport`], which case-folds the scheme, and never
	/// by the spelling of the address — see the builder's docs for what the stricter reading
	/// this function once carried (6c40c86) did to `HTTPS://…`. An unreadable CA path is the
	/// probe: only the TLS branch reads it.
	#[test]
	fn the_bridge_endpoint_negotiates_tls_for_every_spelling_of_https_and_for_nothing_else() {
		let missing = BridgeTlsFiles {
			ca: Some("/nonexistent/piggybank-bridge-ca.pem".to_string()),
			..BridgeTlsFiles::default()
		};
		for addr in ["https://concierge:55672", "HTTPS://concierge:55672", "https://concierge.apps.svc.cluster.local/"] {
			let Err(err) = bridge_endpoint(addr, &missing) else {
				panic!("{addr} must take the TLS branch, which reads the pinned CA");
			};
			assert!(
				err.to_string().contains("BRIDGE_TLS_CA_PEM_FILE"),
				"a CA that cannot be read must name itself and stop the boot, not fall back to the public roots: {err}"
			);
		}
		for addr in ["http://concierge:55670", "http://127.0.0.1:55670"] {
			assert!(bridge_endpoint(addr, &missing).is_ok(), "{addr} is cleartext, so the CA is irrelevant and must not be read");
		}
		assert!(
			bridge_endpoint("https://concierge:55672", &BridgeTlsFiles::default()).is_ok(),
			"an unpinned https seam builds against the public roots; production refuses it earlier, at the boot gate"
		);
		assert!(
			bridge_endpoint("not a url", &BridgeTlsFiles::default()).is_err(),
			"an unusable CONCIERGE_BRIDGE_ADDR must stop the boot"
		);
	}

	/// A pinned CA reaches `config::check_pinned_ca_pem` before it reaches tonic, and a file
	/// that fails it stops the boot instead of building an endpoint with an EMPTY root store.
	/// Which files are trust anchors is settled in `config`'s tests; what this one holds is
	/// the wiring — that the verdict is consulted at all, and that the refusal names the
	/// variable an operator has to go and fix.
	#[test]
	fn a_pinned_bridge_ca_is_parsed_at_boot_or_the_endpoint_is_refused() {
		let good = temp_file(TEST_CA_PEM);
		let accepted = bridge_endpoint("https://concierge:55672", &pinned(&good));
		// Best-effort cleanup: failing to unlink a temp file must not mask the assertion.
		let _ = std::fs::remove_file(&good);
		assert!(accepted.is_ok(), "a readable PEM CA must be accepted as the trust anchor: {:?}", accepted.err());

		let junk = temp_file("not a certificate\n");
		let refused = bridge_endpoint("https://concierge:55672", &pinned(&junk));
		let _ = std::fs::remove_file(&junk);
		let Err(err) = refused else {
			panic!("a CA file holding no certificate must fail the boot, not build an empty root store");
		};
		assert!(err.to_string().contains("BRIDGE_TLS_CA_PEM_FILE"), "the refusal must name the variable that caused it: {err}");
	}

	/// Half an identity presents nothing, so a Secret with one of the two keys written is
	/// refused at boot with the missing variable named — not carried into a handshake that
	/// fails later, against a concierge that asked for a certificate the hub never sent.
	#[test]
	fn a_client_identity_is_both_files_or_neither() {
		let ca = temp_file(TEST_CA_PEM);
		let ca_path = ca.to_str().expect("a utf-8 temp path").to_string();
		let cases = [
			(Some("/nonexistent/client.pem"), None, "BRIDGE_TLS_CLIENT_KEY_PEM_FILE"),
			(None, Some("/nonexistent/client.key"), "BRIDGE_TLS_CLIENT_CERT_PEM_FILE"),
		];
		for (cert, key, missing) in cases {
			let files = BridgeTlsFiles {
				ca: Some(ca_path.clone()),
				client_cert: cert.map(str::to_string),
				client_key: key.map(str::to_string),
			};
			let Err(err) = bridge_endpoint("https://concierge:55672", &files) else {
				panic!("cert {cert:?} with key {key:?} is half an identity and must be refused");
			};
			assert!(err.to_string().contains(missing), "the refusal must name the half that is missing ({missing}): {err}");
		}

		let unreadable = BridgeTlsFiles {
			ca: Some(ca_path),
			client_cert: Some("/nonexistent/client.pem".to_string()),
			client_key: Some("/nonexistent/client.key".to_string()),
		};
		let err = bridge_endpoint("https://concierge:55672", &unreadable).expect_err("an identity that cannot be read stops the boot");
		assert!(err.to_string().contains("BRIDGE_TLS_CLIENT_CERT_PEM_FILE"), "the refusal must name the variable: {err}");
		let _ = std::fs::remove_file(&ca);
	}
}
