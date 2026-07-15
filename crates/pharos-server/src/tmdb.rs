//! T81 — TMDB person-image resolution.
//!
//! This library's `people` rows carry no usable portrait: `thumb_url` is
//! either NULL or a legacy Jellyfin metadata disk path (`/config/data/
//! metadata/People/…`) unreachable from pharos, and the NFO actor parser
//! never captured a per-person TMDB id (so `people.provider_ids` is NULL).
//! The only key we have on a cast member is their **name**, so we resolve
//! portraits via TMDB's person *search* endpoint — the same fallback
//! Jellyfin's own TMDB people-image provider uses.
//!
//! Resolution is a two-step, but only the first step needs the API key:
//!   1. `GET /3/search/person?query=<name>` → the top match's `profile_path`
//!      (needs the key).
//!   2. The portrait itself lives on TMDB's **public** image CDN
//!      (`image.tmdb.org`), so we store the CDN URL on `people.thumb_url`
//!      and let the existing T77 image handler 302 to it — no download, no
//!      cache-PVC storage, always fresh.
//!
//! The whole feature is gated on the key being present ([`TmdbClient`] is
//! only constructed when `[tmdb].api_key` / `PHAROS_TMDB_API_KEY` is set);
//! with no key the backfill never spawns and behaviour is unchanged.

/// TMDB image CDN base for a `w300` (300px-wide) portrait. `profile_path`
/// values already carry a leading `/`, so this concatenates directly.
const IMAGE_BASE_W300: &str = "https://image.tmdb.org/t/p/w300";

/// TMDB REST base.
const API_BASE: &str = "https://api.themoviedb.org/3";

/// Resolves a cast member's portrait URL from their name. Abstracted so the
/// backfill can be driven by a deterministic fake in tests without hitting
/// the network.
pub trait PersonImageResolver {
    /// Return a servable `http(s)` portrait URL for `name`, or `None` when
    /// the provider has no match / errored (the caller leaves the row's
    /// `thumb_url` untouched on `None`).
    fn resolve(&self, name: &str) -> impl std::future::Future<Output = Option<String>> + Send;
}

/// A minimal TMDB v3 client: a shared `reqwest::Client` + the API key.
#[derive(Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    api_key: String,
}

impl TmdbClient {
    /// Build a client around `api_key`. The `reqwest::Client` pools
    /// connections, so one instance is shared across the backfill.
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
        }
    }

    /// Search TMDB for a person by name and return the top match's portrait
    /// CDN URL. `None` on no match, a missing `profile_path`, or any
    /// transport / decode error (person-image enrichment is best-effort —
    /// a TMDB blip must never fail a scan).
    async fn search_person_image(&self, name: &str) -> Option<String> {
        let resp = self
            .http
            .get(format!("{API_BASE}/search/person"))
            .query(&[("api_key", self.api_key.as_str()), ("query", name)])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        parse_profile_path(&body).map(|p| format!("{IMAGE_BASE_W300}{p}"))
    }
}

impl PersonImageResolver for TmdbClient {
    async fn resolve(&self, name: &str) -> Option<String> {
        self.search_person_image(name).await
    }
}

/// Extract the first result's `profile_path` from a TMDB
/// `/search/person` response body. Pulled out (pure, no I/O) so the JSON
/// shape is unit-tested without a live API key. Skips results whose
/// `profile_path` is `null` — TMDB returns name matches with no portrait,
/// and advertising a CDN URL for those would just 404.
fn parse_profile_path(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("results")?
        .as_array()?
        .iter()
        .find_map(|r| r.get("profile_path")?.as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_first_result_profile_path() {
        let body = r#"{"page":1,"results":[
            {"id":1,"name":"Jane Doe","profile_path":"/abc123.jpg"},
            {"id":2,"name":"Jane Doe","profile_path":"/def456.jpg"}
        ]}"#;
        assert_eq!(parse_profile_path(body).as_deref(), Some("/abc123.jpg"));
    }

    #[test]
    fn skips_null_profile_path_to_next_result() {
        let body = r#"{"results":[
            {"id":1,"name":"No Photo","profile_path":null},
            {"id":2,"name":"Has Photo","profile_path":"/real.jpg"}
        ]}"#;
        assert_eq!(parse_profile_path(body).as_deref(), Some("/real.jpg"));
    }

    #[test]
    fn none_when_no_results() {
        assert_eq!(parse_profile_path(r#"{"results":[]}"#), None);
        assert_eq!(parse_profile_path(r#"{"results":[{"id":1}]}"#), None);
    }

    #[test]
    fn none_on_malformed_body() {
        assert_eq!(parse_profile_path("not json"), None);
        assert_eq!(parse_profile_path("{}"), None);
    }

    #[test]
    fn image_base_concatenates_with_leading_slash_path() {
        // profile_path always carries a leading '/', so the CDN URL is a
        // plain concat — guard the format so a stray double-slash regresses.
        assert_eq!(
            format!("{IMAGE_BASE_W300}{}", "/abc.jpg"),
            "https://image.tmdb.org/t/p/w300/abc.jpg"
        );
    }
}
