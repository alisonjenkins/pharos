//! The `Origin` rule for `/pharos/rpc` and `POST /pharos/session`.
//!
//! A WebSocket upgrade is not covered by CORS, and the session cookie rides
//! on it, so without this check any web page a signed-in user visits could
//! open a connection as them (cross-site WebSocket hijacking). Browsers always
//! send `Origin` on both requests and a page cannot forge it; native apps send
//! none and authenticate with a device token instead.

/// Why a request's `Origin` was refused. The label is the `reason` of
/// `pharos_rpc_refused_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginRefusal {
    /// Well-formed, but neither this server nor in `allowed_origins`.
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

/// Allowed when there is no `Origin` (not a browser), when its authority is
/// this request's own `host`, or when it exactly matches an entry of
/// `allowed` (scheme, host and port; case-insensitive; no wildcards).
pub fn check(origin: Option<&str>, host: &str, allowed: &[String]) -> Result<(), OriginRefusal> {
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
    if authority.eq_ignore_ascii_case(host)
        || allowed
            .iter()
            .any(|a| a.trim().trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        Ok(())
    } else {
        Err(OriginRefusal::NotAllowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "pharos.home:8096";

    #[test]
    fn no_origin_is_a_native_client_and_allowed() {
        assert_eq!(check(None, HOST, &[]), Ok(()));
    }

    #[test]
    fn the_servers_own_origin_is_allowed_whatever_the_scheme() {
        assert_eq!(check(Some("http://pharos.home:8096"), HOST, &[]), Ok(()));
        assert_eq!(check(Some("https://PHAROS.home:8096"), HOST, &[]), Ok(()));
    }

    #[test]
    fn a_listed_origin_is_allowed_exactly() {
        let allowed = vec!["https://ui.example.org/".to_string()];
        assert_eq!(
            check(Some("https://ui.example.org"), HOST, &allowed),
            Ok(())
        );
        assert_eq!(
            check(Some("http://ui.example.org"), HOST, &allowed),
            Err(OriginRefusal::NotAllowed),
            "scheme is part of the match"
        );
        assert_eq!(
            check(Some("https://evil.ui.example.org"), HOST, &allowed),
            Err(OriginRefusal::NotAllowed)
        );
    }

    #[test]
    fn another_site_is_refused() {
        assert_eq!(
            check(Some("https://evil.example"), HOST, &[]),
            Err(OriginRefusal::NotAllowed)
        );
        assert_eq!(
            check(Some("http://pharos.home:8097"), HOST, &[]),
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
            assert_eq!(
                check(Some(o), HOST, &[]),
                Err(OriginRefusal::Malformed),
                "{o:?}"
            );
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
