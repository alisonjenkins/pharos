//! Outbound HTTP clients (metadata providers, lyrics, remote sources).
//!
//! reqwest 0.13 builds rustls with aws-lc-rs and the OS certificate verifier
//! by default. Pharos keeps ring (no C build) and the bundled webpki roots it
//! used under 0.12, so every outbound client starts from [`builder`], which
//! hands reqwest a preconfigured rustls config. A plain `Client::new()` would
//! panic: with `rustls-no-provider` there is no crypto provider to fall back on.

use std::sync::OnceLock;

/// Built once: the root store copy and provider setup are the same for every
/// client.
fn shared_tls_config() -> Option<&'static rustls::ClientConfig> {
    static CONFIG: OnceLock<Option<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| match pharos_api::tls::client_config() {
            Ok(config) => Some(config),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "outbound TLS config failed to build; clients fall back to reqwest's verifier with ring as the process default"
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
    fn shared_config_is_built() {
        assert!(shared_tls_config().is_some());
    }

    #[test]
    fn builder_builds_without_a_process_default_provider() {
        // `rustls-no-provider` makes reqwest panic when nothing supplies a
        // provider; the preconfigured path must not need one.
        assert!(builder().build().is_ok());
    }
}
