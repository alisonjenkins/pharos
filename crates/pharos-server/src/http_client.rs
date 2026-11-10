//! Outbound HTTP clients (metadata providers, lyrics, remote sources).
//!
//! reqwest 0.13 builds rustls with aws-lc-rs and the OS certificate verifier
//! by default. Pharos keeps ring (no C build) and the bundled webpki roots it
//! used under 0.12, so every outbound client starts from [`builder`], which
//! hands reqwest a preconfigured rustls config. A plain `Client::new()` would
//! panic: with `rustls-no-provider` there is no crypto provider to fall back on.

use std::sync::{Arc, OnceLock};

/// The rustls config every outbound client shares: ring, rustls's default
/// protocol versions, the webpki root set, and ALPN for HTTP/1.1 (the only
/// version the workspace enables).
fn tls_config() -> Result<rustls::ClientConfig, rustls::Error> {
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

/// Built once: the root store copy and provider setup are the same for every
/// client.
fn shared_tls_config() -> Option<&'static rustls::ClientConfig> {
    static CONFIG: OnceLock<Option<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| match tls_config() {
            Ok(config) => Some(config),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "outbound TLS config failed to build; installing ring as the process default instead"
                );
                None
            }
        })
        .as_ref()
}

/// A `reqwest::ClientBuilder` with Pharos's TLS settings applied. Start every
/// outbound client here instead of `reqwest::Client::builder()`.
pub fn builder() -> reqwest::ClientBuilder {
    match shared_tls_config() {
        Some(config) => reqwest::Client::builder().tls_backend_preconfigured(config.clone()),
        None => {
            // Cannot happen with ring's protocol list, but if it ever did,
            // reqwest would panic without a process-default provider.
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
        }
    }
}

/// A client with Pharos's TLS settings and reqwest's other defaults; the
/// replacement for `reqwest::Client::new()`.
#[must_use]
pub fn client() -> reqwest::Client {
    builder().build().unwrap_or_else(|error| {
        tracing::error!(error = %error, "outbound HTTP client failed to build; using reqwest defaults");
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_config_uses_ring_webpki_roots_and_http1_alpn() {
        let config = tls_config().unwrap_or_else(|e| panic!("tls config: {e}"));
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert_eq!(
            config.crypto_provider().cipher_suites,
            rustls::crypto::ring::default_provider().cipher_suites
        );
    }

    #[test]
    fn builder_builds_without_a_process_default_provider() {
        // `rustls-no-provider` makes reqwest panic when nothing supplies a
        // provider; the preconfigured path must not need one.
        assert!(builder().build().is_ok());
    }
}
