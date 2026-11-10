//! The rustls client config shared by Pharos's native HTTP clients.
//!
//! reqwest 0.13's own rustls setup uses aws-lc-rs (a C build) and
//! rustls-platform-verifier (which needs JNI setup on Android). Pharos hands
//! reqwest this config instead, so the server and the Android TV app verify
//! servers the same way 0.12's `rustls-tls` did: ring and the webpki roots.

use std::sync::Arc;

/// ring, rustls's safe default protocol versions, the webpki root set, and
/// ALPN for HTTP/1.1 (the only HTTP version the workspace enables in reqwest).
///
/// # Errors
///
/// Only if ring rejects rustls's default protocol versions, which it does
/// not; callers still handle it rather than panic.
pub fn client_config() -> Result<rustls::ClientConfig, rustls::Error> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_uses_ring_webpki_roots_and_http1_alpn() {
        let config = client_config().unwrap_or_else(|e| panic!("tls config: {e}"));
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert_eq!(
            config.crypto_provider().cipher_suites,
            rustls::crypto::ring::default_provider().cipher_suites
        );
    }
}
