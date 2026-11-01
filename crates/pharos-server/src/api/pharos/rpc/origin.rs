//! The `Origin` rule for `/pharos/rpc` and `POST /pharos/session`.
//!
//! A WebSocket upgrade is not covered by CORS, and the session cookie rides
//! on it, so without this check any web page a signed-in user visits could
//! open a connection as them (cross-site WebSocket hijacking). Browsers always
//! send `Origin` on both requests and a page cannot forge it; native apps send
//! none and authenticate with a device token instead.
//!
//! The request's own `Host` is deliberately not trusted as "same origin": under
//! DNS rebinding an attacker's page sends `Origin` and `Host` both naming the
//! attacker's domain. Browser origins are only ever the configured
//! `allowed_origins`, including the one pharos-ui itself is served from.

/// Why a request's `Origin` was refused. The label is the `reason` of
/// `pharos_rpc_refused_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginRefusal {
    /// Well-formed, but not in `allowed_origins`.
    NotAllowed,
    /// Not `scheme://host[:port]` (includes the opaque origin `null`).
    Malformed,
}

impl OriginRefusal {
    pub fn label(self) -> &'static str {
        match self {
            OriginRefusal::NotAllowed => "origin_not_allowed",
            OriginRefusal::Malformed => "origin_malformed",
        }
    }
}

/// Allowed when there is no `Origin` (not a browser), or when it exactly
/// matches an entry of `allowed` (scheme, host and port; case-insensitive; no
/// wildcards).
pub fn check(origin: Option<&str>, allowed: &[String]) -> Result<(), OriginRefusal> {
    let Some(origin) = origin else {
        return Ok(());
    };
    let origin = origin.trim();
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or(OriginRefusal::Malformed)?;
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', ' ']) {
        return Err(OriginRefusal::Malformed);
    }
    if allowed
        .iter()
        .any(|a| a.trim().trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        Ok(())
    } else {
        Err(OriginRefusal::NotAllowed)
    }
}

/// Entries of `allowed` a browser cannot keep the `Secure` session cookie
/// on: plain `http://` other than a loopback host, which browsers treat as
/// secure.
pub fn insecure(allowed: &[String]) -> Vec<&str> {
    allowed
        .iter()
        .map(|a| a.trim())
        .filter(|a| {
            let Some(authority) = a.strip_prefix("http://") else {
                return false;
            };
            let host = authority
                .rsplit_once(':')
                .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
                .map_or(authority, |(host, _)| host)
                .trim_end_matches('/');
            !(host.eq_ignore_ascii_case("localhost")
                || host.to_ascii_lowercase().ends_with(".localhost")
                || host == "127.0.0.1"
                || host == "[::1]")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_insecure_except_on_loopback() {
        let allowed = [
            "https://pharos.example.org",
            "http://192.168.1.10:8096",
            "http://pharos.lan",
            "http://localhost:8080",
            "http://127.0.0.1",
            "http://[::1]:3000",
            "http://ui.localhost",
        ]
        .map(String::from);
        assert_eq!(
            insecure(&allowed),
            ["http://192.168.1.10:8096", "http://pharos.lan"]
        );
    }

    #[test]
    fn no_origin_is_a_native_client_and_allowed() {
        assert_eq!(check(None, &[]), Ok(()));
    }

    /// DNS rebinding: the attacker's page names its own domain in both
    /// `Origin` and `Host`, so "matches the Host header" proves nothing.
    #[test]
    fn an_origin_matching_the_host_header_is_not_enough() {
        assert_eq!(
            check(Some("http://attacker.example:8096"), &[]),
            Err(OriginRefusal::NotAllowed)
        );
    }

    #[test]
    fn a_listed_origin_is_allowed_exactly() {
        let allowed = vec!["https://ui.example.org/".to_string()];
        assert_eq!(check(Some("https://ui.example.org"), &allowed), Ok(()));
        assert_eq!(
            check(Some("http://ui.example.org"), &allowed),
            Err(OriginRefusal::NotAllowed),
            "scheme is part of the match"
        );
        assert_eq!(
            check(Some("https://evil.ui.example.org"), &allowed),
            Err(OriginRefusal::NotAllowed)
        );
    }

    #[test]
    fn another_site_is_refused() {
        assert_eq!(
            check(Some("https://evil.example"), &[]),
            Err(OriginRefusal::NotAllowed)
        );
        assert_eq!(
            check(Some("http://pharos.home:8097"), &[]),
            Err(OriginRefusal::NotAllowed),
            "port is part of the match"
        );
    }

    #[test]
    fn opaque_or_odd_origins_are_malformed() {
        for o in [
            "null",
            "",
            "file://",
            "https://",
            "https://a/b",
            "https://u@pharos.home:8096",
        ] {
            assert_eq!(check(Some(o), &[]), Err(OriginRefusal::Malformed), "{o:?}");
        }
    }

    #[test]
    fn refusal_labels_are_distinct() {
        assert_ne!(
            OriginRefusal::NotAllowed.label(),
            OriginRefusal::Malformed.label()
        );
    }
}
