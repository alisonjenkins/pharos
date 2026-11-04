//! Cast portraits served from this origin. The native UI's CSP allows images
//! from `'self'` only, so a portrait hosted by a metadata provider is fetched
//! by the server and passed through instead of redirected to. Only the
//! providers' own image hosts are fetched: a `thumb_url` comes from NFO files
//! too, and must not make the server request an arbitrary address.

use std::sync::OnceLock;
use std::time::Duration;

/// Image hosts of the providers that supply portraits (TMDB, TVDB).
const PORTRAIT_HOSTS: [&str; 2] = ["image.tmdb.org", "artworks.thetvdb.com"];

/// A portrait slower than this is dropped; the UI shows initials instead.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Larger than any headshot a provider serves at the sizes pharos asks for.
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// Upstream fetches in flight at once. A title page asks for up to 40
/// portraits together; this keeps one page view from opening 40 connections
/// to the provider.
const MAX_IN_FLIGHT: usize = 8;

/// Whether `url` is a provider portrait this server fetches on a client's
/// behalf: https, on one of [`PORTRAIT_HOSTS`].
pub fn is_proxied(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https" && u.host_str().is_some_and(|h| PORTRAIT_HOSTS.contains(&h))
    })
}

/// A fetched portrait: its content type and bytes.
pub struct Portrait {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Fetch the portrait at `url`. `None` when it is not [`is_proxied`], or on
/// any transport error, non-success status, non-image body or oversized body.
pub async fn fetch(url: &str) -> Option<Portrait> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    static IN_FLIGHT: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    if !is_proxied(url) {
        return None;
    }
    let _permit = IN_FLIGHT
        .get_or_init(|| tokio::sync::Semaphore::new(MAX_IN_FLIGHT))
        .acquire()
        .await
        .ok()?;
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default()
    });
    let resp = match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => resp,
        Ok(resp) => {
            tracing::debug!(
                url,
                status = resp.status().as_u16(),
                "portrait: upstream refused"
            );
            return None;
        }
        Err(e) => {
            tracing::debug!(url, error = %e, "portrait: fetch failed");
            return None;
        }
    };
    let Some(content_type) = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|t| t.starts_with("image/"))
        .map(str::to_string)
    else {
        tracing::debug!(url, "portrait: upstream body is not an image");
        return None;
    };
    let mut resp = resp;
    let mut bytes = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= MAX_BYTES => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(Some(_)) => {
                tracing::debug!(url, max = MAX_BYTES, "portrait: body too large");
                return None;
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(url, error = %e, "portrait: body read failed");
                return None;
            }
        }
    }
    Some(Portrait {
        content_type,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::is_proxied;

    #[test]
    fn only_provider_image_hosts_over_https_are_fetched() {
        assert!(is_proxied("https://image.tmdb.org/t/p/w185/abc.jpg"));
        assert!(is_proxied(
            "https://artworks.thetvdb.com/banners/person/1/a.jpg"
        ));
        assert!(!is_proxied("http://image.tmdb.org/t/p/w185/abc.jpg"));
        assert!(!is_proxied("https://image.tmdb.org.evil.example/a.jpg"));
        assert!(!is_proxied("https://10.0.0.1/a.jpg"));
        assert!(!is_proxied("/config/metadata/People/a.jpg"));
    }
}
