//! Cast portraits served from this origin. The native UI's CSP allows images
//! from `'self'` only, so a portrait hosted by a metadata provider is fetched
//! by the server and passed through instead of redirected to. Only the
//! providers' own image hosts are fetched: a `thumb_url` comes from NFO files
//! too, and must not make the server request an arbitrary address.
//!
//! The upstream fetch sits behind [`PortraitSource`] (ADR-0076) so the HTTP
//! client can be swapped, and faked in tests.

use std::time::Duration;

use futures_util::future::BoxFuture;

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

/// How long a fetch waits for one of the [`MAX_IN_FLIGHT`] slots. A title
/// page's 40 portraits clear the slots in about five rounds of fast fetches;
/// past this, a burst is dropped instead of queueing upstream traffic behind it.
const SLOT_WAIT: Duration = Duration::from_secs(5);

/// Whether `url` is a provider portrait this server fetches on a client's
/// behalf: https, on one of [`PORTRAIT_HOSTS`].
pub fn is_proxied(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https" && u.host_str().is_some_and(|h| PORTRAIT_HOSTS.contains(&h))
    })
}

/// A fetched portrait: its content type and bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portrait {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Why a portrait fetch gave nothing. Its [`label`](Self::label) is the
/// `outcome` of `pharos_portrait_fetch_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortraitMiss {
    /// The url is not on a provider image host over https.
    NotProxied,
    Transport,
    Status(u16),
    NotImage,
    TooLarge,
    /// Every fetch slot stayed taken for [`SLOT_WAIT`].
    Busy,
}

impl PortraitMiss {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotProxied => "not_proxied",
            Self::Transport => "transport",
            Self::Status(_) => "status",
            Self::NotImage => "not_image",
            Self::TooLarge => "too_large",
            Self::Busy => "busy",
        }
    }
}

/// Where portrait bytes come from.
pub trait PortraitSource: Send + Sync {
    /// The portrait at `url`, which the caller has checked with [`is_proxied`].
    fn fetch<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Portrait, PortraitMiss>>;
}

/// Fetch through `source`, recording the outcome. `None` on any miss.
pub async fn fetch(source: &dyn PortraitSource, url: &str) -> Option<Portrait> {
    let result = if is_proxied(url) {
        source.fetch(url).await
    } else {
        Err(PortraitMiss::NotProxied)
    };
    let outcome = match &result {
        Ok(_) => "ok",
        Err(miss) => miss.label(),
    };
    metrics::counter!("pharos_portrait_fetch_total", "outcome" => outcome).increment(1);
    match result {
        Ok(p) => Some(p),
        Err(miss) => {
            tracing::debug!(url, outcome, ?miss, "portrait: no upstream image");
            None
        }
    }
}

/// [`PortraitSource`] over HTTPS with `reqwest`: a timeout, no redirects, a
/// size cap, and at most [`MAX_IN_FLIGHT`] requests at once.
pub struct HttpPortraits {
    client: reqwest::Client,
    in_flight: tokio::sync::Semaphore,
}

impl HttpPortraits {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            client,
            in_flight: tokio::sync::Semaphore::new(MAX_IN_FLIGHT),
        }
    }

    async fn get(&self, url: &str) -> Result<Portrait, PortraitMiss> {
        let _permit = take_slot(&self.in_flight, SLOT_WAIT).await?;
        let mut resp = self.client.get(url).send().await.map_err(|e| {
            tracing::debug!(url, error = %e, "portrait: fetch failed");
            PortraitMiss::Transport
        })?;
        if !resp.status().is_success() {
            return Err(PortraitMiss::Status(resp.status().as_u16()));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|t| t.starts_with("image/"))
            .map(str::to_string)
            .ok_or(PortraitMiss::NotImage)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| {
            tracing::debug!(url, error = %e, "portrait: body read failed");
            PortraitMiss::Transport
        })? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(PortraitMiss::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Portrait {
            content_type,
            bytes,
        })
    }
}

async fn take_slot(
    slots: &tokio::sync::Semaphore,
    wait: Duration,
) -> Result<tokio::sync::SemaphorePermit<'_>, PortraitMiss> {
    match tokio::time::timeout(wait, slots.acquire()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_closed)) => Err(PortraitMiss::Transport),
        Err(_elapsed) => Err(PortraitMiss::Busy),
    }
}

impl Default for HttpPortraits {
    fn default() -> Self {
        Self::new()
    }
}

impl PortraitSource for HttpPortraits {
    fn fetch<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Portrait, PortraitMiss>> {
        Box::pin(self.get(url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(AtomicUsize);

    impl PortraitSource for Counting {
        fn fetch<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<Portrait, PortraitMiss>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Portrait {
                    content_type: "image/jpeg".into(),
                    bytes: vec![1, 2, 3],
                })
            })
        }
    }

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

    #[tokio::test]
    async fn a_url_off_the_provider_hosts_never_reaches_the_source() {
        let source = Counting(AtomicUsize::new(0));
        assert!(fetch(&source, "https://evil.example/a.jpg").await.is_none());
        assert_eq!(source.0.load(Ordering::SeqCst), 0);
        let got = fetch(&source, "https://image.tmdb.org/t/p/w185/a.jpg").await;
        assert_eq!(got.map(|p| p.bytes), Some(vec![1, 2, 3]));
        assert_eq!(source.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_fetch_that_cannot_get_a_slot_in_time_is_busy() {
        let slots = tokio::sync::Semaphore::new(1);
        let held = take_slot(&slots, SLOT_WAIT).await;
        assert!(held.is_ok());
        let waited = take_slot(&slots, Duration::from_millis(10)).await;
        assert_eq!(waited.err(), Some(PortraitMiss::Busy));
        drop(held);
        assert!(take_slot(&slots, Duration::from_millis(10)).await.is_ok());
    }

    #[test]
    fn miss_labels_are_distinct() {
        let labels = [
            PortraitMiss::NotProxied,
            PortraitMiss::Transport,
            PortraitMiss::Status(404),
            PortraitMiss::NotImage,
            PortraitMiss::TooLarge,
            PortraitMiss::Busy,
        ]
        .map(PortraitMiss::label);
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }
}
