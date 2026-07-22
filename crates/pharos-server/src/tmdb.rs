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

/// TMDB image CDN base for full-resolution art (posters/backdrops/stills
/// downloaded for the local artwork cache, as opposed to the `w300`
/// portrait thumbnail used for the inline 302 redirect above).
const IMAGE_BASE_ORIGINAL: &str = "https://image.tmdb.org/t/p/original";

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

    /// Search TMDB movies by title (optionally narrowed by release year).
    /// Empty `Vec` on any transport/HTTP/decode error — search is
    /// best-effort, never fails the caller.
    pub(crate) async fn search_movie(
        &self,
        query: &str,
        year: Option<u32>,
    ) -> Vec<pharos_core::SearchCandidate> {
        let mut params = vec![
            ("api_key", self.api_key.clone()),
            ("query", query.to_string()),
        ];
        if let Some(y) = year {
            params.push(("year", y.to_string()));
        }
        let Ok(resp) = self
            .http
            .get(format!("{API_BASE}/search/movie"))
            .query(&params)
            .send()
            .await
        else {
            return vec![];
        };
        if !resp.status().is_success() {
            return vec![];
        }
        let Ok(body) = resp.text().await else {
            return vec![];
        };
        parse_movie_search(&body)
    }

    /// Search TMDB TV series by name (optionally narrowed by first-air
    /// year). Empty `Vec` on any transport/HTTP/decode error.
    pub(crate) async fn search_tv(
        &self,
        query: &str,
        year: Option<u32>,
    ) -> Vec<pharos_core::SearchCandidate> {
        let mut params = vec![
            ("api_key", self.api_key.clone()),
            ("query", query.to_string()),
        ];
        if let Some(y) = year {
            params.push(("first_air_date_year", y.to_string()));
        }
        let Ok(resp) = self
            .http
            .get(format!("{API_BASE}/search/tv"))
            .query(&params)
            .send()
            .await
        else {
            return vec![];
        };
        if !resp.status().is_success() {
            return vec![];
        }
        let Ok(body) = resp.text().await else {
            return vec![];
        };
        parse_tv_search(&body)
    }

    /// Fetch a TMDB movie's detail record by id.
    pub(crate) async fn movie_detail(
        &self,
        id: &str,
    ) -> Option<crate::online_enrich::EnrichedMetadata> {
        let resp = self
            .http
            .get(format!("{API_BASE}/movie/{id}"))
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        parse_movie_detail(&body)
    }

    /// Fetch a TMDB TV series' detail record by id.
    pub(crate) async fn tv_detail(
        &self,
        id: &str,
    ) -> Option<crate::online_enrich::EnrichedMetadata> {
        let resp = self
            .http
            .get(format!("{API_BASE}/tv/{id}"))
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        parse_tv_detail(&body)
    }

    /// Fetch a single TMDB TV episode's detail record by series id +
    /// season/episode number.
    pub(crate) async fn episode_detail(
        &self,
        series_id: &str,
        season: u32,
        episode: u32,
    ) -> Option<crate::online_enrich::EnrichedMetadata> {
        let resp = self
            .http
            .get(format!(
                "{API_BASE}/tv/{series_id}/season/{season}/episode/{episode}"
            ))
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        parse_episode_detail(&body)
    }

    /// Download the raw bytes of an artwork/image URL (typically one built
    /// from an `IMAGE_BASE_ORIGINAL` + `*_path` pair in an
    /// [`crate::online_enrich::EnrichedMetadata`]). `None` on any
    /// transport/HTTP error — a failed image fetch must never fail the
    /// enrichment pass as a whole.
    pub async fn fetch_image_bytes(&self, url: &str) -> Option<Vec<u8>> {
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.bytes().await.ok().map(|b| b.to_vec())
    }
}

/// [`crate::online_enrich::OnlineEnricher`] impl backed by [`TmdbClient`].
/// TMDB covers both movies and TV (episodes fetch via the series id +
/// season/episode), and doubles as the artwork CDN for TVDB gap-fill (T6)
/// via [`crate::online_enrich::EnrichedMetadata::also_tmdb_id`].
pub struct TmdbEnricher(pub TmdbClient);

impl crate::online_enrich::OnlineEnricher for TmdbEnricher {
    fn provider(&self) -> &'static str {
        "tmdb"
    }

    fn supports(&self, _kind: pharos_core::MediaKind) -> bool {
        true
    }

    async fn search(
        &self,
        kind: pharos_core::MediaKind,
        title: &str,
        year: Option<u32>,
    ) -> Vec<pharos_core::SearchCandidate> {
        match kind {
            pharos_core::MediaKind::Movie => self.0.search_movie(title, year).await,
            pharos_core::MediaKind::Episode => self.0.search_tv(title, year).await,
            pharos_core::MediaKind::Audio => vec![],
        }
    }

    async fn fetch(
        &self,
        kind: pharos_core::MediaKind,
        id: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Option<crate::online_enrich::EnrichedMetadata> {
        match kind {
            pharos_core::MediaKind::Movie => self.0.movie_detail(id).await,
            pharos_core::MediaKind::Episode => match (season, episode) {
                (Some(s), Some(e)) => self.0.episode_detail(id, s, e).await,
                _ => self.0.tv_detail(id).await,
            },
            pharos_core::MediaKind::Audio => None,
        }
    }

    async fn fetch_image_bytes(&self, url: &str) -> Option<Vec<u8>> {
        self.0.fetch_image_bytes(url).await
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

/// Extract the leading `YYYY` from a TMDB `release_date` / `first_air_date`
/// / `air_date` string (`"2021-10-01"` → `2021`). `None` on anything that
/// doesn't parse as a 4-digit year (empty string, malformed date, etc).
fn year_of(date: &str) -> Option<u32> {
    date.get(0..4)?.parse().ok()
}

/// Parse a TMDB `/search/movie` response body into ranked
/// [`pharos_core::SearchCandidate`]s. Pure (no I/O) so the JSON shape is
/// unit-tested without a live API key; malformed JSON or a missing
/// `results` array yields an empty `Vec` rather than panicking (search is
/// best-effort).
pub(crate) fn parse_movie_search(body: &str) -> Vec<pharos_core::SearchCandidate> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    v.get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let id = r.get("id")?.as_i64()?.to_string();
                    let title = r.get("title")?.as_str()?.to_string();
                    let year = r
                        .get("release_date")
                        .and_then(|d| d.as_str())
                        .and_then(year_of);
                    Some(pharos_core::SearchCandidate { id, title, year })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a TMDB `/search/tv` response body into ranked
/// [`pharos_core::SearchCandidate`]s. Mirrors [`parse_movie_search`] but TV
/// results carry `name`/`first_air_date` instead of `title`/
/// `release_date`.
pub(crate) fn parse_tv_search(body: &str) -> Vec<pharos_core::SearchCandidate> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    v.get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let id = r.get("id")?.as_i64()?.to_string();
                    let title = r.get("name")?.as_str()?.to_string();
                    let year = r
                        .get("first_air_date")
                        .and_then(|d| d.as_str())
                        .and_then(year_of);
                    Some(pharos_core::SearchCandidate { id, title, year })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a TMDB `/movie/{id}` detail response body into
/// [`crate::online_enrich::EnrichedMetadata`]. `None` on malformed JSON —
/// a decode failure is treated the same as "no data" by the caller.
pub(crate) fn parse_movie_detail(body: &str) -> Option<crate::online_enrich::EnrichedMetadata> {
    use crate::online_enrich::{EnrichedMetadata, RemoteArt};
    use pharos_core::ArtworkRole;
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut art = vec![];
    if let Some(p) = v.get("poster_path").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Primary,
            url: format!("{IMAGE_BASE_ORIGINAL}{p}"),
        });
    }
    if let Some(b) = v.get("backdrop_path").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Backdrop,
            url: format!("{IMAGE_BASE_ORIGINAL}{b}"),
        });
    }
    Some(EnrichedMetadata {
        overview: v
            .get("overview")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        production_year: v
            .get("release_date")
            .and_then(|x| x.as_str())
            .and_then(year_of),
        premiere_date: v
            .get("release_date")
            .and_then(|x| x.as_str())
            .and_then(pharos_core::parse_ymd_to_unix),
        community_rating: v
            .get("vote_average")
            .and_then(|x| x.as_f64())
            .map(|f| f as f32),
        genres: v
            .get("genres")
            .and_then(|g| g.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|g| g.get("name")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        provider_id: v.get("id").and_then(|x| x.as_i64()).map(|i| i.to_string()),
        artwork: art,
        ..EnrichedMetadata::default()
    })
}

/// Parse a TMDB `/tv/{id}` detail response body into
/// [`crate::online_enrich::EnrichedMetadata`]. Mirrors
/// [`parse_movie_detail`] but TV series detail carries `first_air_date`
/// instead of `release_date`.
pub(crate) fn parse_tv_detail(body: &str) -> Option<crate::online_enrich::EnrichedMetadata> {
    use crate::online_enrich::{EnrichedMetadata, RemoteArt};
    use pharos_core::ArtworkRole;
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut art = vec![];
    if let Some(p) = v.get("poster_path").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Primary,
            url: format!("{IMAGE_BASE_ORIGINAL}{p}"),
        });
    }
    if let Some(b) = v.get("backdrop_path").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Backdrop,
            url: format!("{IMAGE_BASE_ORIGINAL}{b}"),
        });
    }
    Some(EnrichedMetadata {
        overview: v
            .get("overview")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        production_year: v
            .get("first_air_date")
            .and_then(|x| x.as_str())
            .and_then(year_of),
        premiere_date: v
            .get("first_air_date")
            .and_then(|x| x.as_str())
            .and_then(pharos_core::parse_ymd_to_unix),
        community_rating: v
            .get("vote_average")
            .and_then(|x| x.as_f64())
            .map(|f| f as f32),
        genres: v
            .get("genres")
            .and_then(|g| g.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|g| g.get("name")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        provider_id: v.get("id").and_then(|x| x.as_i64()).map(|i| i.to_string()),
        artwork: art,
        ..EnrichedMetadata::default()
    })
}

/// Parse a TMDB `/tv/{id}/season/{s}/episode/{e}` detail response body
/// into [`crate::online_enrich::EnrichedMetadata`]. Episode responses
/// carry `name` (the episode title), `overview`, `air_date` (bare
/// `YYYY-MM-DD`, parsed to unix seconds via
/// [`pharos_core::parse_ymd_to_unix`]), and `still_path` (a single
/// per-episode still, mapped to [`pharos_core::ArtworkRole::Thumb`]).
pub(crate) fn parse_episode_detail(body: &str) -> Option<crate::online_enrich::EnrichedMetadata> {
    use crate::online_enrich::{EnrichedMetadata, RemoteArt};
    use pharos_core::ArtworkRole;
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut art = vec![];
    if let Some(s) = v.get("still_path").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Thumb,
            url: format!("{IMAGE_BASE_ORIGINAL}{s}"),
        });
    }
    Some(EnrichedMetadata {
        title: v
            .get("name")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        overview: v
            .get("overview")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        premiere_date: v
            .get("air_date")
            .and_then(|x| x.as_str())
            .and_then(pharos_core::parse_ymd_to_unix),
        community_rating: v
            .get("vote_average")
            .and_then(|x| x.as_f64())
            .map(|f| f as f32),
        provider_id: v.get("id").and_then(|x| x.as_i64()).map(|i| i.to_string()),
        artwork: art,
        ..EnrichedMetadata::default()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
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

    #[test]
    fn tmdb_parse_search_results_yields_candidates() {
        let body = r#"{"results":[
            {"id":438631,"title":"Dune","release_date":"2021-10-01"},
            {"id":438632,"title":"Dune Part Two","release_date":"2024-03-01"}]}"#;
        let c = super::parse_movie_search(body);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "438631");
        assert_eq!(c[0].year, Some(2021));
    }

    #[test]
    fn tmdb_parse_movie_detail_extracts_overview_genres_art() {
        let body = r#"{"id":438631,"overview":"A duke's son...","release_date":"2021-10-01",
            "vote_average":7.8,"genres":[{"name":"Science Fiction"},{"name":"Adventure"}],
            "poster_path":"/p.jpg","backdrop_path":"/b.jpg"}"#;
        let e = super::parse_movie_detail(body).unwrap();
        assert_eq!(e.overview.as_deref(), Some("A duke's son..."));
        assert_eq!(e.genres, vec!["Science Fiction", "Adventure"]);
        assert_eq!(e.community_rating, Some(7.8));
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == pharos_core::ArtworkRole::Primary && a.url.ends_with("/p.jpg")));
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == pharos_core::ArtworkRole::Backdrop));
        // release_date "2021-10-01" -> unix seconds at UTC midnight.
        assert_eq!(e.premiere_date, Some(1_633_046_400));
    }

    #[test]
    fn tmdb_parse_movie_detail_none_on_malformed_body() {
        assert!(super::parse_movie_detail("not json").is_none());
    }

    #[test]
    fn tmdb_parse_tv_search_yields_candidates_from_name_and_first_air_date() {
        let body = r#"{"results":[
            {"id":1396,"name":"Breaking Bad","first_air_date":"2008-01-20"},
            {"id":94997,"name":"House of the Dragon","first_air_date":"2022-08-21"}]}"#;
        let c = super::parse_tv_search(body);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "1396");
        assert_eq!(c[0].title, "Breaking Bad");
        assert_eq!(c[0].year, Some(2008));
    }

    #[test]
    fn tmdb_parse_tv_detail_extracts_overview_genres_art() {
        let body = r#"{"id":1396,"overview":"A high school chemistry teacher...",
            "first_air_date":"2008-01-20","vote_average":8.9,
            "genres":[{"name":"Drama"},{"name":"Crime"}],
            "poster_path":"/tv_p.jpg","backdrop_path":"/tv_b.jpg"}"#;
        let e = super::parse_tv_detail(body).unwrap();
        assert_eq!(
            e.overview.as_deref(),
            Some("A high school chemistry teacher...")
        );
        assert_eq!(e.production_year, Some(2008));
        assert_eq!(e.genres, vec!["Drama", "Crime"]);
        assert_eq!(e.community_rating, Some(8.9));
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == pharos_core::ArtworkRole::Primary && a.url.ends_with("/tv_p.jpg")));
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == pharos_core::ArtworkRole::Backdrop && a.url.ends_with("/tv_b.jpg")));
        // first_air_date "2008-01-20" -> unix seconds at UTC midnight.
        assert_eq!(e.premiere_date, Some(1_200_787_200));
    }

    #[test]
    fn tmdb_parse_tv_detail_none_on_malformed_body() {
        assert!(super::parse_tv_detail("not json").is_none());
    }

    #[test]
    fn tmdb_parse_episode_detail_extracts_overview_title_still() {
        let body = r#"{"id":62085,"name":"Pilot","overview":"Walter White, a struggling...",
            "air_date":"2008-01-20","vote_average":8.2,"still_path":"/still.jpg"}"#;
        let e = super::parse_episode_detail(body).unwrap();
        assert_eq!(e.title.as_deref(), Some("Pilot"));
        assert_eq!(e.overview.as_deref(), Some("Walter White, a struggling..."));
        assert_eq!(e.community_rating, Some(8.2));
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == pharos_core::ArtworkRole::Thumb && a.url.ends_with("/still.jpg")));
        // air_date "2008-01-20" -> unix seconds at UTC midnight (Task 11.5).
        assert_eq!(e.premiere_date, Some(1_200_787_200));
    }

    #[test]
    fn tmdb_parse_episode_detail_none_on_malformed_body() {
        assert!(super::parse_episode_detail("not json").is_none());
    }
}
