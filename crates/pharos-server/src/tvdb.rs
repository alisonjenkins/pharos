//! T6 — TVDB v4 client + enricher.
//!
//! TVDB v4 authenticates with a short-lived JWT rather than a per-request
//! API key (unlike TMDB, see `tmdb.rs`): `POST /login {"apikey": ...}` hands
//! back a bearer token that must be attached to every subsequent request and
//! re-minted on expiry (surfaced as a `401`). That auth state-machine is the
//! risky part of this module, so it is built over a small [`TvdbTransport`]
//! trait rather than calling `reqwest` directly — [`TvdbClient`] is generic
//! over the transport, letting the login-caching / re-login-on-401 logic be
//! driven by a network-free fake in tests (see `tests::FakeTransport`) while
//! [`ReqwestTransport`] is the real HTTP impl used in production.
//!
//! TVDB is TV-only here: [`TvdbEnricher::supports`] returns `true` only for
//! [`pharos_core::MediaKind::Episode`] — TVDB has no useful movie/audio
//! catalog for this feature, TMDB already covers those (T5).

use std::future::Future;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::online_enrich::{EnrichedMetadata, RemoteArt};
use pharos_core::{ArtworkRole, MediaKind, SearchCandidate};

/// TVDB v4 REST base.
const API_BASE: &str = "https://api4.thetvdb.com/v4";

/// Abstraction over the HTTP transport TVDB calls ride on. Exists solely so
/// [`TvdbClient`]'s auth state-machine (cache the JWT, re-login once on a
/// `401`) is unit-testable without a network — a fake impl in `tests` queues
/// canned responses and counts `login` calls; [`ReqwestTransport`] is the
/// real impl used outside tests. RPITIT (no `async-trait`, no new
/// dependency); a `dyn` object isn't needed because [`TvdbClient`] is
/// generic over `T: TvdbTransport` instead.
pub trait TvdbTransport: Send + Sync {
    /// `GET {API_BASE}{path}` with `Authorization: Bearer {bearer}`.
    /// Returns `(http_status, body)`; a transport-level failure (no
    /// connection, decode error, ...) is reported as status `0` with an
    /// empty body so callers can treat it uniformly as "not 200".
    fn get(&self, path: &str, bearer: &str) -> impl Future<Output = (u16, String)> + Send;

    /// `POST {API_BASE}/login {"apikey": apikey}`, returning the minted
    /// bearer token from `.data.token`. `None` on any transport/HTTP/decode
    /// failure or a missing token field.
    fn login(&self, apikey: &str) -> impl Future<Output = Option<String>> + Send;

    /// Download raw bytes from an absolute, already-CDN-qualified URL (no
    /// bearer — TVDB's artwork CDN is public). `None` on any transport/HTTP
    /// error.
    fn fetch_bytes(&self, url: &str) -> impl Future<Output = Option<Vec<u8>>> + Send;
}

/// The real [`TvdbTransport`] impl, wrapping a shared `reqwest::Client`.
#[derive(Clone)]
pub struct ReqwestTransport {
    http: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl TvdbTransport for ReqwestTransport {
    async fn get(&self, path: &str, bearer: &str) -> (u16, String) {
        let Ok(resp) = self
            .http
            .get(format!("{API_BASE}{path}"))
            .bearer_auth(bearer)
            .send()
            .await
        else {
            return (0, String::new());
        };
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        (status, body)
    }

    async fn login(&self, apikey: &str) -> Option<String> {
        let resp = self
            .http
            .post(format!("{API_BASE}/login"))
            .json(&serde_json::json!({ "apikey": apikey }))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        v.get("data")?.get("token")?.as_str().map(str::to_string)
    }

    async fn fetch_bytes(&self, url: &str) -> Option<Vec<u8>> {
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.bytes().await.ok().map(|b| b.to_vec())
    }
}

/// A TVDB v4 client generic over its [`TvdbTransport`]. Holds the API key,
/// the transport, and the cached JWT behind an `Arc<RwLock<..>>` so it can
/// be cloned/shared across concurrent callers (the enrichment orchestrator,
/// T9, will fan out over many items) while sharing one cached token.
#[derive(Clone)]
pub struct TvdbClient<T: TvdbTransport> {
    api_key: String,
    transport: T,
    token: Arc<RwLock<Option<String>>>,
}

impl TvdbClient<ReqwestTransport> {
    /// Build a client around `api_key` using the real `reqwest` transport.
    pub fn new(api_key: String) -> Self {
        Self::with_transport(api_key, ReqwestTransport::new())
    }
}

impl<T: TvdbTransport> TvdbClient<T> {
    /// Build a client around an explicit transport — the production
    /// constructor for [`ReqwestTransport`], or a test fake.
    pub fn with_transport(api_key: String, transport: T) -> Self {
        Self {
            api_key,
            transport,
            token: Arc::new(RwLock::new(None)),
        }
    }

    /// Return the cached JWT, or log in and cache a freshly minted one.
    /// `None` if login fails (bad key, network down, ...).
    async fn ensure_token(&self) -> Option<String> {
        if let Some(t) = self.token.read().await.clone() {
            return Some(t);
        }
        let t = self.transport.login(&self.api_key).await?;
        *self.token.write().await = Some(t.clone());
        Some(t)
    }

    /// `GET path`, transparently handling the JWT: use the cached token,
    /// and on a `401` clear it and re-login exactly once before retrying.
    /// `None` if login fails, the retry also fails, or the final status
    /// isn't `200`.
    async fn authed_get(&self, path: &str) -> Option<String> {
        let mut token = self.ensure_token().await?;
        let (status, body) = self.transport.get(path, &token).await;
        if status == 401 {
            *self.token.write().await = None;
            token = self.ensure_token().await?;
            let (status2, body2) = self.transport.get(path, &token).await;
            if status2 != 200 {
                return None;
            }
            return Some(body2);
        }
        if status != 200 {
            return None;
        }
        Some(body)
    }

    /// Search TVDB for a series by name (optionally narrowed by year).
    /// Empty `Vec` on any auth/transport/decode failure — search is
    /// best-effort, never fails the caller.
    pub async fn search_series(&self, query: &str, year: Option<u32>) -> Vec<SearchCandidate> {
        let mut path = format!("/search?query={}&type=series", encode_query(query));
        if let Some(y) = year {
            path.push_str(&format!("&year={y}"));
        }
        let Some(body) = self.authed_get(&path).await else {
            return vec![];
        };
        parse_series_search(&body)
    }

    /// Fetch a TVDB series' extended detail record by id.
    pub async fn get_series(&self, id: &str) -> Option<EnrichedMetadata> {
        let body = self.authed_get(&format!("/series/{id}/extended")).await?;
        parse_series_detail(&body)
    }

    /// Fetch a single episode's detail by series id + season/episode
    /// number, via the series' episode list.
    pub async fn get_episode(
        &self,
        series_id: &str,
        season: u32,
        episode: u32,
    ) -> Option<EnrichedMetadata> {
        let body = self
            .authed_get(&format!("/series/{series_id}/episodes/default"))
            .await?;
        parse_episode_detail(&body, season, episode)
    }

    /// Download the raw bytes of a TVDB artwork/image CDN URL. `None` on
    /// any transport/HTTP error.
    pub async fn fetch_image_bytes(&self, url: &str) -> Option<Vec<u8>> {
        self.transport.fetch_bytes(url).await
    }
}

/// Percent-encode a query-string value byte-by-byte (UTF-8 safe: each byte
/// of a multi-byte character is escaped independently, which is standard
/// percent-encoding behaviour). No new dependency for what's otherwise a
/// one-off need — titles are the only thing ever encoded here.
fn encode_query(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse a TVDB `/search?type=series` response body into ranked
/// [`SearchCandidate`]s. Pure (no I/O) so the JSON shape is unit-tested
/// without a live API key; malformed JSON or a missing `data` array yields
/// an empty `Vec` rather than panicking (search is best-effort). TVDB
/// carries `year` as a string, unlike TMDB's date strings.
pub(crate) fn parse_series_search(body: &str) -> Vec<SearchCandidate> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let id = r.get("tvdb_id")?.as_str()?.to_string();
                    let title = r.get("name")?.as_str()?.to_string();
                    let year = r
                        .get("year")
                        .and_then(|y| y.as_str())
                        .and_then(|s| s.parse::<u32>().ok());
                    Some(SearchCandidate { id, title, year })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Extract the TMDB id from a TVDB `remoteIds` array (`.remoteIds[]` with
/// `sourceName`/`id`), so callers get a bridge to TMDB's artwork CDN for a
/// title TVDB itself has weaker imagery for. `None` if the array is absent
/// or carries no `"TheMovieDB"` entry.
fn extract_tmdb_remote_id(data: &serde_json::Value) -> Option<String> {
    data.get("remoteIds")?.as_array()?.iter().find_map(|r| {
        if r.get("sourceName").and_then(|s| s.as_str()) == Some("TheMovieDB") {
            r.get("id").and_then(|id| id.as_str()).map(str::to_string)
        } else {
            None
        }
    })
}

/// Parse a TVDB `/series/{id}/extended` detail response body into
/// [`EnrichedMetadata`]. `None` only on malformed JSON or a missing `data`
/// object — a bare `{"data":{"id":..,"name":..}}` fixture with no
/// overview/genres/image still yields `Some` with whatever fields are
/// present, mirroring `tmdb::parse_movie_detail`'s tolerance.
/// `premiere_date` is parsed from the series' `firstAired` field (bare
/// `YYYY-MM-DD`, TVDB v4's series-level air-date field) via
/// [`pharos_core::parse_ymd_to_unix`].
pub(crate) fn parse_series_detail(body: &str) -> Option<EnrichedMetadata> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let data = v.get("data")?;
    let mut art = vec![];
    if let Some(img) = data.get("image").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Primary,
            url: img.to_string(),
        });
    }
    Some(EnrichedMetadata {
        title: data
            .get("name")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        overview: data
            .get("overview")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        production_year: data
            .get("year")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok()),
        premiere_date: data
            .get("firstAired")
            .and_then(|x| x.as_str())
            .and_then(pharos_core::parse_ymd_to_unix),
        genres: data
            .get("genres")
            .and_then(|g| g.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|g| g.get("name")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        provider_id: data
            .get("id")
            .and_then(|x| x.as_i64())
            .map(|i| i.to_string()),
        also_tmdb_id: extract_tmdb_remote_id(data),
        artwork: art,
        ..EnrichedMetadata::default()
    })
}

/// Parse a TVDB `/series/{id}/episodes/default` list response body,
/// picking out the entry matching `season`/`episode` and mapping it to
/// [`EnrichedMetadata`]. `None` if the JSON is malformed, the `data.episodes`
/// array is missing, or no episode matches the requested season/number.
/// `premiere_date` is parsed from the episode's `aired` field (bare
/// `YYYY-MM-DD`, TVDB v4's per-episode air-date field) via
/// [`pharos_core::parse_ymd_to_unix`].
pub(crate) fn parse_episode_detail(
    body: &str,
    season: u32,
    episode: u32,
) -> Option<EnrichedMetadata> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let episodes = v.get("data")?.get("episodes")?.as_array()?;
    let ep = episodes.iter().find(|e| {
        let s = e.get("seasonNumber").and_then(|x| x.as_u64());
        let n = e.get("number").and_then(|x| x.as_u64());
        s == Some(u64::from(season)) && n == Some(u64::from(episode))
    })?;
    let mut art = vec![];
    if let Some(img) = ep.get("image").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Thumb,
            url: img.to_string(),
        });
    }
    Some(EnrichedMetadata {
        title: ep
            .get("name")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        overview: ep
            .get("overview")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        premiere_date: ep
            .get("aired")
            .and_then(|x| x.as_str())
            .and_then(pharos_core::parse_ymd_to_unix),
        provider_id: ep.get("id").and_then(|x| x.as_i64()).map(|i| i.to_string()),
        also_tmdb_id: extract_tmdb_remote_id(ep),
        artwork: art,
        ..EnrichedMetadata::default()
    })
}

/// [`crate::online_enrich::OnlineEnricher`] impl backed by [`TvdbClient`].
/// TVDB is TV-only: `supports` gates to [`MediaKind::Episode`], so the
/// orchestrator (T9) skips this provider for movies/audio and relies on
/// TMDB (T5) there instead.
pub struct TvdbEnricher<T: TvdbTransport>(pub TvdbClient<T>);

impl<T: TvdbTransport> crate::online_enrich::OnlineEnricher for TvdbEnricher<T> {
    fn provider(&self) -> &'static str {
        "tvdb"
    }

    fn supports(&self, kind: MediaKind) -> bool {
        kind == MediaKind::Episode
    }

    async fn search(
        &self,
        kind: MediaKind,
        title: &str,
        year: Option<u32>,
    ) -> Vec<SearchCandidate> {
        match kind {
            MediaKind::Episode => self.0.search_series(title, year).await,
            MediaKind::Movie | MediaKind::Audio => vec![],
        }
    }

    async fn fetch(
        &self,
        kind: MediaKind,
        id: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Option<EnrichedMetadata> {
        match kind {
            MediaKind::Episode => match (season, episode) {
                (Some(s), Some(e)) => self.0.get_episode(id, s, e).await,
                _ => self.0.get_series(id).await,
            },
            MediaKind::Movie | MediaKind::Audio => None,
        }
    }

    async fn fetch_image_bytes(&self, url: &str) -> Option<Vec<u8>> {
        self.0.fetch_image_bytes(url).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A network-free [`TvdbTransport`] fake: queues canned `login`/`get`
    /// responses and counts `login` calls, so the auth state-machine
    /// (cache the JWT, re-login exactly once on a `401`) is verifiable
    /// without a real TVDB server. `Clone`s share the same queues/counter
    /// (`Arc`-backed) so the test can hand a clone to the client while
    /// keeping a handle to assert on afterwards.
    #[derive(Clone, Default)]
    struct FakeTransport {
        logins: Arc<Mutex<VecDeque<String>>>,
        gets: Arc<Mutex<VecDeque<(u16, String)>>>,
        login_count: Arc<AtomicUsize>,
    }

    impl FakeTransport {
        fn new() -> Self {
            Self::default()
        }

        fn push_login(self, token: &str) -> Self {
            self.logins
                .lock()
                .expect("lock")
                .push_back(token.to_string());
            self
        }

        fn push_ok(self, body: &str) -> Self {
            self.gets
                .lock()
                .expect("lock")
                .push_back((200, body.to_string()));
            self
        }

        fn push_401(self) -> Self {
            self.gets
                .lock()
                .expect("lock")
                .push_back((401, String::new()));
            self
        }

        fn login_count(&self) -> usize {
            self.login_count.load(Ordering::SeqCst)
        }
    }

    impl TvdbTransport for FakeTransport {
        async fn get(&self, _path: &str, _bearer: &str) -> (u16, String) {
            self.gets
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or((0, String::new()))
        }

        async fn login(&self, _apikey: &str) -> Option<String> {
            self.login_count.fetch_add(1, Ordering::SeqCst);
            self.logins.lock().expect("lock").pop_front()
        }

        async fn fetch_bytes(&self, _url: &str) -> Option<Vec<u8>> {
            None
        }
    }

    #[tokio::test]
    async fn tvdb_jwt_cached_and_relogin_on_401() {
        let t = FakeTransport::new()
            .push_login("jwt-1")
            .push_ok(r#"{"data":{"id":121361,"name":"Game of Thrones"}}"#) // first call ok
            .push_401() // token expired
            .push_login("jwt-2")
            .push_ok(r#"{"data":{"id":121361,"name":"Game of Thrones"}}"#); // retried ok
        let c = TvdbClient::with_transport("key".into(), t.clone());
        assert!(c.get_series("121361").await.is_some());
        assert!(c.get_series("121361").await.is_some());
        assert_eq!(t.login_count(), 2); // logged in once, re-logged once after 401
    }

    #[tokio::test]
    async fn tvdb_login_failure_yields_none_without_panicking() {
        let t = FakeTransport::new(); // no queued login token
        let c = TvdbClient::with_transport("key".into(), t);
        assert!(c.get_series("121361").await.is_none());
    }

    #[tokio::test]
    async fn tvdb_non_200_after_relogin_yields_none() {
        let t = FakeTransport::new()
            .push_login("jwt-1")
            .push_401()
            .push_login("jwt-2")
            .push_401(); // still failing after re-login
        let c = TvdbClient::with_transport("key".into(), t.clone());
        assert!(c.get_series("121361").await.is_none());
        assert_eq!(t.login_count(), 2);
    }

    #[test]
    fn tvdb_parse_series_search_yields_candidates() {
        let body = r#"{"data":[{"tvdb_id":"121361","name":"Game of Thrones","year":"2011"}]}"#;
        let c = super::parse_series_search(body);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].id, "121361");
        assert_eq!(c[0].title, "Game of Thrones");
        assert_eq!(c[0].year, Some(2011));
    }

    #[test]
    fn tvdb_parse_series_search_none_on_malformed_body() {
        assert!(super::parse_series_search("not json").is_empty());
        assert!(super::parse_series_search("{}").is_empty());
    }

    #[test]
    fn tvdb_parse_series_detail_extracts_fields_and_remote_tmdb_id() {
        let body = r#"{"data":{"id":121361,"name":"Game of Thrones","overview":"Nine noble families...",
            "year":"2011","firstAired":"2011-04-17","image":"https://artworks.thetvdb.com/banners/posters/121361.jpg",
            "genres":[{"name":"Drama"},{"name":"Fantasy"}],
            "remoteIds":[{"sourceName":"IMDB","id":"tt0944947"},{"sourceName":"TheMovieDB","id":"1399"}]}}"#;
        let e = super::parse_series_detail(body).unwrap();
        assert_eq!(e.title.as_deref(), Some("Game of Thrones"));
        assert_eq!(e.overview.as_deref(), Some("Nine noble families..."));
        assert_eq!(e.production_year, Some(2011));
        assert_eq!(e.genres, vec!["Drama", "Fantasy"]);
        assert_eq!(e.provider_id.as_deref(), Some("121361"));
        assert_eq!(e.also_tmdb_id.as_deref(), Some("1399"));
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == ArtworkRole::Primary && a.url.ends_with("121361.jpg")));
        // firstAired "2011-04-17" -> unix seconds at UTC midnight.
        assert_eq!(e.premiere_date, Some(1_302_998_400));
    }

    #[test]
    fn tvdb_parse_series_detail_tolerates_sparse_fixture() {
        let body = r#"{"data":{"id":121361,"name":"Game of Thrones"}}"#;
        let e = super::parse_series_detail(body).unwrap();
        assert_eq!(e.title.as_deref(), Some("Game of Thrones"));
        assert_eq!(e.provider_id.as_deref(), Some("121361"));
        assert_eq!(e.also_tmdb_id, None);
        assert!(e.artwork.is_empty());
        assert_eq!(e.premiere_date, None); // no firstAired in this sparse fixture
    }

    #[test]
    fn tvdb_parse_series_detail_none_on_malformed_body() {
        assert!(super::parse_series_detail("not json").is_none());
        assert!(super::parse_series_detail("{}").is_none());
    }

    #[test]
    fn tvdb_parse_episode_detail_extracts_matching_episode() {
        let body = r#"{"data":{"episodes":[
            {"id":1,"seasonNumber":1,"number":1,"name":"Winter Is Coming","overview":"Lord Stark...","aired":"2011-04-17","image":"https://artworks.thetvdb.com/ep1.jpg"},
            {"id":2,"seasonNumber":1,"number":2,"name":"The Kingsroad","overview":"While Bran...","aired":"2011-04-24"}
        ]}}"#;
        let e = super::parse_episode_detail(body, 1, 1).unwrap();
        assert_eq!(e.title.as_deref(), Some("Winter Is Coming"));
        assert_eq!(e.overview.as_deref(), Some("Lord Stark..."));
        assert_eq!(e.provider_id.as_deref(), Some("1"));
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == ArtworkRole::Thumb && a.url.ends_with("ep1.jpg")));
        // aired "2011-04-17" -> unix seconds at UTC midnight.
        assert_eq!(e.premiere_date, Some(1_302_998_400));

        let e2 = super::parse_episode_detail(body, 1, 2).unwrap();
        // aired "2011-04-24" -> unix seconds at UTC midnight.
        assert_eq!(e2.premiere_date, Some(1_303_603_200));
    }

    #[test]
    fn tvdb_parse_episode_detail_none_when_no_match() {
        let body = r#"{"data":{"episodes":[{"id":1,"seasonNumber":1,"number":1,"name":"Winter Is Coming"}]}}"#;
        assert!(super::parse_episode_detail(body, 9, 9).is_none());
    }

    #[test]
    fn tvdb_parse_episode_detail_none_on_malformed_body() {
        assert!(super::parse_episode_detail("not json", 1, 1).is_none());
        assert!(super::parse_episode_detail("{}", 1, 1).is_none());
    }

    #[test]
    fn tvdb_enricher_supports_episode_only() {
        let e = TvdbEnricher(TvdbClient::with_transport(
            "key".to_string(),
            FakeTransport::new(),
        ));
        use crate::online_enrich::OnlineEnricher;
        assert!(e.supports(MediaKind::Episode));
        assert!(!e.supports(MediaKind::Movie));
        assert!(!e.supports(MediaKind::Audio));
        assert_eq!(e.provider(), "tvdb");
    }

    #[test]
    fn encode_query_escapes_spaces_and_keeps_alnum() {
        assert_eq!(
            super::encode_query("Game of Thrones"),
            "Game%20of%20Thrones"
        );
        assert_eq!(super::encode_query("abc-123_.~"), "abc-123_.~");
    }
}
