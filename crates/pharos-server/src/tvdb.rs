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

use crate::metadata_language::{record_translation, TranslationOutcome, DEFAULT_LANGUAGE};
use crate::online_enrich::{EnrichedMetadata, RemoteArt, TextsFetch};
use pharos_core::{ArtworkRole, LocalizedText, MediaKind, SearchCandidate};

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
        match self.authed_get_status(path).await? {
            (200, body) => Some(body),
            _ => None,
        }
    }

    /// A record body for the text fetch: 200 is the body, 404 is
    /// [`TextsFetch::NotFound`], anything else [`TextsFetch::Failed`].
    async fn record_body(&self, path: &str) -> Result<String, TextsFetch> {
        match self.authed_get_status(path).await {
            Some((200, body)) => Ok(body),
            Some((404, _)) => Err(TextsFetch::NotFound),
            _ => Err(TextsFetch::Failed),
        }
    }

    /// [`authed_get`](Self::authed_get) that hands back the final status
    /// with the body, so a caller can tell "TVDB has no such record" (404)
    /// from a failed request. `None` only when no token could be had.
    async fn authed_get_status(&self, path: &str) -> Option<(u16, String)> {
        let token = self.ensure_token().await?;
        let (status, body) = self.transport.get(path, &token).await;
        if status != 401 {
            return Some((status, body));
        }
        *self.token.write().await = None;
        let token = self.ensure_token().await?;
        Some(self.transport.get(path, &token).await)
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

    /// Fetch a TVDB series' extended detail record by id, with its title and
    /// overview in English when TVDB has them (B564): the base record is in
    /// the language the show was made in, which for anime is Japanese.
    pub async fn get_series(&self, id: &str) -> Option<EnrichedMetadata> {
        let mut md = self
            .series_record(id, &[DEFAULT_LANGUAGE.to_string()])
            .await
            .ok()?;
        prefer_language(&mut md, DEFAULT_LANGUAGE);
        Some(md)
    }

    /// The series record with `texts` holding each of `languages` plus the
    /// original. `?meta=translations` inlines every translation TVDB has, so
    /// this is one request whatever the languages.
    /// A body that does not parse is [`TextsFetch::Failed`]: TVDB answered,
    /// but not with a record.
    async fn series_record(
        &self,
        id: &str,
        languages: &[String],
    ) -> Result<EnrichedMetadata, TextsFetch> {
        let body = self
            .record_body(&format!("/series/{id}/extended?meta=translations"))
            .await?;
        let mut md = parse_series_detail(&body).ok_or(TextsFetch::Failed)?;
        let all = parse_series_texts(&body);
        md.texts = pick_languages(all, languages, |lang, outcome| {
            record_translation("tvdb", lang, outcome);
            if outcome == TranslationOutcome::Missing {
                tracing::info!(
                    tvdb.series_id = id,
                    language = lang,
                    reason = "series has no translation in this language",
                    "tvdb text falls back to the default text"
                );
            }
        });
        Ok(md)
    }

    /// Fetch a single episode's detail by series id + season/episode
    /// number, via the series' episode list, with its name and overview in
    /// English when TVDB has them (B564). `name`, the episode's own name,
    /// overrules a number TVDB counts differently (B570).
    pub async fn get_episode(
        &self,
        series_id: &str,
        season: u32,
        episode: u32,
        name: Option<&str>,
    ) -> Option<EnrichedMetadata> {
        let (mut md, _) = self
            .episode_record(
                series_id,
                season,
                episode,
                name,
                &[DEFAULT_LANGUAGE.to_string()],
            )
            .await
            .ok()?;
        prefer_language(&mut md, DEFAULT_LANGUAGE);
        Some(md)
    }

    /// The episode record with `texts` holding each of `languages` plus the
    /// original. The episode list names the languages each episode has
    /// (`nameTranslations`/`overviewTranslations`), so a language it does not
    /// list is recorded missing without a request; one it lists costs one
    /// `/episodes/{id}/translations/{lang}`. The flag is true when one of
    /// those requests failed, so its language has no row. An episode the
    /// list does not hold is [`TextsFetch::NotFound`].
    async fn episode_record(
        &self,
        series_id: &str,
        season: u32,
        episode: u32,
        name: Option<&str>,
        languages: &[String],
    ) -> Result<(EnrichedMetadata, bool), TextsFetch> {
        let body = self
            .record_body(&format!("/series/{series_id}/episodes/default"))
            .await?;
        let Some(mut md) = parse_episode_detail(&body, season, episode, name) else {
            // Only a readable list that lacks the episode is "no such
            // record"; a body that is not a list (cut off in transit, say)
            // is a failed request, as in `series_record`.
            return Err(if episode_list_readable(&body) {
                TextsFetch::NotFound
            } else {
                TextsFetch::Failed
            });
        };
        // The record picked, which `name` may have moved off `episode`.
        let (season, episode) = md.episode_number.unwrap_or((season, episode));
        let langs = parse_episode_languages(&body, season, episode);
        let mut texts = Vec::new();
        let mut failed = false;
        if let Some(orig) = &langs.original {
            texts.push(LocalizedText {
                language: orig.clone(),
                title: md.title.clone(),
                overview: md.overview.clone(),
                is_original: true,
            });
        }
        for lang in languages {
            let fallback = |reason: &str| {
                tracing::info!(
                    tvdb.series_id = series_id,
                    tvdb.episode_id = md.provider_id.as_deref().unwrap_or_default(),
                    season,
                    episode,
                    language = lang.as_str(),
                    reason,
                    "tvdb text falls back to the default text"
                );
            };
            if langs.original.as_deref() == Some(lang.as_str()) {
                record_translation("tvdb", lang, TranslationOutcome::Original);
                continue;
            }
            let ep_id = md
                .provider_id
                .as_deref()
                .filter(|_| langs.listed.contains(lang));
            let Some(ep_id) = ep_id else {
                record_translation("tvdb", lang, TranslationOutcome::Missing);
                fallback("episode lists no translation in this language");
                texts.push(missing_text(lang));
                continue;
            };
            let path = format!("/episodes/{ep_id}/translations/{lang}");
            match self.authed_get_status(&path).await {
                Some((200, body)) => match parse_translation(&body) {
                    Some(t) => {
                        record_translation("tvdb", lang, TranslationOutcome::Translated);
                        texts.push(LocalizedText {
                            language: lang.clone(),
                            ..t
                        });
                    }
                    None => {
                        record_translation("tvdb", lang, TranslationOutcome::Missing);
                        fallback("translation record carried no text");
                        texts.push(missing_text(lang));
                    }
                },
                Some((404, _)) => {
                    record_translation("tvdb", lang, TranslationOutcome::Missing);
                    fallback("translation request answered 404");
                    texts.push(missing_text(lang));
                }
                other => {
                    failed = true;
                    record_translation("tvdb", lang, TranslationOutcome::Error);
                    let status = other.map(|(s, _)| s.to_string());
                    tracing::warn!(
                        tvdb.series_id = series_id,
                        tvdb.episode_id = ep_id,
                        season,
                        episode,
                        language = lang.as_str(),
                        status = status.as_deref().unwrap_or("no token"),
                        "tvdb translation request failed; text falls back to the default text \
                         and is asked again next pass"
                    );
                }
            }
        }
        md.texts = texts;
        Ok((md, failed))
    }

    /// Download the raw bytes of a TVDB artwork/image CDN URL. `None` on
    /// any transport/HTTP error.
    pub async fn fetch_image_bytes(&self, url: &str) -> Option<Vec<u8>> {
        self.transport.fetch_bytes(url).await
    }

    /// List a series' artworks from its extended record (the same endpoint
    /// [`get_series`](Self::get_series) reads). Empty on any
    /// auth/transport/decode failure.
    pub async fn list_series_artworks(&self, id: &str) -> Vec<crate::online_enrich::RemoteImage> {
        let Some(body) = self.authed_get(&format!("/series/{id}/extended")).await else {
            tracing::warn!(
                series_id = id,
                "tvdb list_series_artworks: no response body (auth/transport failure)"
            );
            return vec![];
        };
        let images = parse_tvdb_artworks(&body);
        if images.is_empty() {
            tracing::debug!(
                series_id = id,
                "tvdb list_series_artworks: provider returned no usable images"
            );
        }
        images
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
/// carries `year` as a string, unlike TMDB's date strings. `name` is the
/// show's ORIGINAL-language title (`ドラゴンボール` for Dragon Ball), so the
/// English `translations.eng` and the `aliases` ride along as the candidate's
/// aliases for [`pharos_core::match_best`] (B557).
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
                    let title = decode_entities(r.get("name")?.as_str()?);
                    let year = r
                        .get("year")
                        .and_then(|y| y.as_str())
                        .and_then(|s| s.parse::<u32>().ok());
                    let english = r
                        .get("translations")
                        .and_then(|t| t.get("eng"))
                        .and_then(|e| e.as_str());
                    let aliases = english
                        .into_iter()
                        .chain(
                            r.get("aliases")
                                .and_then(|a| a.as_array())
                                .into_iter()
                                .flatten()
                                .filter_map(|a| a.as_str()),
                        )
                        .map(decode_entities)
                        .collect();
                    Some(SearchCandidate {
                        id,
                        title,
                        year,
                        aliases,
                    })
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
/// TVDB v4 series artwork `type` ids the picker surfaces. Others (icon=5,
/// banner=1, season art, …) are dropped — only poster/backdrop/logo.
fn tvdb_artwork_role(type_id: i64) -> Option<ArtworkRole> {
    match type_id {
        2 => Some(ArtworkRole::Primary),  // poster
        3 => Some(ArtworkRole::Backdrop), // background
        23 => Some(ArtworkRole::Logo),    // clearlogo
        _ => None,
    }
}

/// Parse a TVDB `/series/{id}/extended` body's `data.artworks[]` into
/// role-tagged candidates. Empty on any decode error / missing array.
pub(crate) fn parse_tvdb_artworks(body: &str) -> Vec<crate::online_enrich::RemoteImage> {
    use crate::online_enrich::RemoteImage;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let Some(arr) = v
        .get("data")
        .and_then(|d| d.get("artworks"))
        .and_then(|a| a.as_array())
    else {
        return vec![];
    };
    let mut out = vec![];
    for a in arr {
        let Some(role) = a
            .get("type")
            .and_then(|x| x.as_i64())
            .and_then(tvdb_artwork_role)
        else {
            continue;
        };
        let Some(url) = a.get("image").and_then(|x| x.as_str()) else {
            continue;
        };
        out.push(RemoteImage {
            role,
            url: url.to_string(),
            width: a.get("width").and_then(|x| x.as_u64()).map(|n| n as u32),
            height: a.get("height").and_then(|x| x.as_u64()).map(|n| n as u32),
            language: a
                .get("language")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            community_rating: a.get("score").and_then(|x| x.as_f64()).map(|f| f as f32),
            vote_count: None,
        });
    }
    out
}

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
    // B125 — `data.image` is the poster and nothing else, so a show could never
    // acquire a backdrop or a logo: 0 of 178 shows in the deployed library had
    // one. The SAME response already carries `data.artworks[]` (this is the
    // `/series/{id}/extended` body `parse_tvdb_artworks` reads for the image
    // picker), so take the best-scored candidate per missing role from it
    // rather than fetching anything more.
    let mut candidates = parse_tvdb_artworks(body);
    candidates.sort_by(|a, b| {
        b.community_rating
            .unwrap_or(0.0)
            .total_cmp(&a.community_rating.unwrap_or(0.0))
    });
    for role in [
        ArtworkRole::Backdrop,
        ArtworkRole::Logo,
        ArtworkRole::Primary,
    ] {
        if art.iter().any(|a: &RemoteArt| a.role == role) {
            continue;
        }
        if let Some(best) = candidates.iter().find(|c| c.role == role) {
            art.push(RemoteArt {
                role,
                url: best.url.clone(),
            });
        }
    }
    Some(EnrichedMetadata {
        title: data
            .get("name")
            .and_then(|x| x.as_str())
            .map(decode_entities),
        overview: data
            .get("overview")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(decode_entities),
        production_year: data
            .get("year")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok()),
        premiere_date: data
            .get("firstAired")
            .and_then(|x| x.as_str())
            .and_then(pharos_core::parse_ymd_to_unix),
        // TVDB reports this on the SERIES record only. Episodes inherit it —
        // it is what makes `OriginalLanguage` work for anime, whose audio
        // preference is the whole reason the field is captured.
        original_language: data
            .get("originalLanguage")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
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

/// Decode the HTML character references TVDB leaves in its text fields
/// (search names included, so `R&amp;R` can still match a query exactly), once:
/// its JSON carries `Dragon Ball R&amp;R` for "R&R", and the UI escapes what
/// it is given, so the reference showed on screen (B558). A single left-to-
/// right pass, so `&amp;lt;` becomes `&lt;` rather than `<`. An unknown or
/// unterminated reference is kept as written.
pub(crate) fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest
            .get(1..)
            .and_then(|r| {
                r.find(';')
                    .filter(|&end| end <= MAX_ENTITY_LEN)
                    .map(|end| &r[..end])
            })
            .and_then(|name| Some((entity_char(name)?, name.len() + 2)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Longest reference body [`decode_entities`] looks for (`#x10FFFF`), so a
/// stray `&` far from any `;` is not scanned to the end of an overview.
const MAX_ENTITY_LEN: usize = 8;

fn entity_char(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Parse a TVDB `/series/{id}/episodes/default` list response body,
/// picking out the entry matching `season`/`episode` and mapping it to
/// [`EnrichedMetadata`]. `None` if the JSON is malformed, the `data.episodes`
/// array is missing, or no episode matches the requested season/number.
/// `premiere_date` is parsed from the episode's `aired` field (bare
/// `YYYY-MM-DD`, TVDB v4's per-episode air-date field) via
/// [`pharos_core::parse_ymd_to_unix`].
/// Whether `body` is a readable episode list (`data.episodes` an array).
fn episode_list_readable(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("data")?.get("episodes")?.as_array().map(|_| ()))
        .is_some()
}

pub(crate) fn parse_episode_detail(
    body: &str,
    season: u32,
    episode: u32,
    name: Option<&str>,
) -> Option<EnrichedMetadata> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let episodes = v.get("data")?.get("episodes")?.as_array()?;
    let ep = pick_episode(episodes, season, episode, name)?;
    let mut art = vec![];
    // B125 — an episode's still is its PRIMARY in Jellyfin. Filed under Thumb
    // it left the episode with no poster, and the image route answered with an
    // arbitrary extracted frame instead.
    if let Some(img) = ep.get("image").and_then(|x| x.as_str()) {
        art.push(RemoteArt {
            role: ArtworkRole::Primary,
            url: img.to_string(),
        });
    }
    Some(EnrichedMetadata {
        title: ep
            .get("name")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(decode_entities),
        overview: ep
            .get("overview")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(decode_entities),
        premiere_date: ep
            .get("aired")
            .and_then(|x| x.as_str())
            .and_then(pharos_core::parse_ymd_to_unix),
        provider_id: ep.get("id").and_then(|x| x.as_i64()).map(|i| i.to_string()),
        also_tmdb_id: extract_tmdb_remote_id(ep),
        artwork: art,
        episode_number: episode_number(ep),
        ..EnrichedMetadata::default()
    })
}

/// Lowest [`title_similarity`] at which an episode's own name, from its
/// filename, names a record in the provider's list (B570). High, because it
/// overrules the episode number; a part suffix must also agree (see
/// [`same_numbers`]).
const EPISODE_NAME_MIN_SIMILARITY: f32 = 0.9;

/// How far from its numbered slot, within its season, an episode's named
/// record may sit. A renumbering shifts episodes by one or two (a two-part
/// pilot counted as one); a generic name further away is another episode.
const EPISODE_NAME_MAX_SHIFT: u64 = 3;

/// The record for episode `season`/`episode` of a series' episode list,
/// cross-checked against the episode's own `name` (B570).
///
/// The file's `SxxEyy` and the provider can count a season differently: TVDB
/// now lists The Next Generation's two-part pilot as one episode, so every
/// later episode sits one number lower than in files named before the
/// change, and "S01E03 - The Naked Now" is TVDB's S01E02. When the numbered
/// record's name does not resemble `name` and exactly one record nearby in
/// the same season does, that record is the episode. A `name` that is a
/// stand-in ("Episode 6", a release tag) matches nothing and leaves the
/// numbered record in place, as does a tie.
fn pick_episode<'a>(
    episodes: &'a [serde_json::Value],
    season: u32,
    episode: u32,
    name: Option<&str>,
) -> Option<&'a serde_json::Value> {
    let in_season = |e: &&serde_json::Value| {
        e.get("seasonNumber").and_then(|x| x.as_u64()) == Some(u64::from(season))
    };
    let numbered = episodes
        .iter()
        .filter(in_season)
        .find(|e| e.get("number").and_then(|x| x.as_u64()) == Some(u64::from(episode)));
    let Some(name) = name.filter(|n| names_an_episode(n)) else {
        return numbered;
    };
    let named = |e: &serde_json::Value| -> f32 {
        let Some(record) = e.get("name").and_then(|x| x.as_str()).map(decode_entities) else {
            return 0.0;
        };
        if !same_numbers(name, &record) {
            return 0.0;
        }
        pharos_core::title_similarity(name, &record)
    };
    if numbered.is_some_and(|e| named(e) >= EPISODE_NAME_MIN_SIMILARITY) {
        return numbered;
    }
    let mut near: Vec<(f32, &serde_json::Value)> = episodes
        .iter()
        .filter(in_season)
        .filter(|e| {
            e.get("number").and_then(|x| x.as_u64()).is_some_and(|n| {
                n != u64::from(episode) && n.abs_diff(u64::from(episode)) <= EPISODE_NAME_MAX_SHIFT
            })
        })
        .map(|e| (named(e), e))
        .filter(|(score, _)| *score >= EPISODE_NAME_MIN_SIMILARITY)
        .collect();
    near.sort_by(|a, b| b.0.total_cmp(&a.0));
    match near.as_slice() {
        [(best, ep), rest @ ..] if rest.first().map_or(true, |(next, _)| next < best) => {
            tracing::info!(
                season,
                episode,
                name,
                numbered_name = numbered
                    .and_then(|e| e.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or(""),
                record_number = ep.get("number").and_then(|x| x.as_u64()).unwrap_or(0),
                record_id = ep.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
                "tvdb episode re-identified by its name: the provider numbers it differently"
            );
            Some(*ep)
        }
        _ => numbered,
    }
}

/// Whether `name` can identify an episode: it has words, and is not a
/// numbered stand-in such as "Episode 6" or "Part 1".
fn names_an_episode(name: &str) -> bool {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let letters = name.chars().filter(|c| c.is_alphabetic()).count();
    let stand_in = matches!(
        words.as_slice(),
        [w, n] if ["episode", "ep", "part", "chapter"].contains(&w.to_lowercase().as_str())
            && n.chars().all(|c| c.is_ascii_digit())
    );
    letters >= 3 && !stand_in
}

/// Whether `a` and `b` carry the same numbers ("Graduation Day (2)" is not
/// "Graduation Day (1)", though the two read 94% alike).
fn same_numbers(a: &str, b: &str) -> bool {
    let numbers = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|n| !n.is_empty())
            .map(|n| n.trim_start_matches('0').to_string())
            .collect()
    };
    numbers(a) == numbers(b)
}

/// A list record's `(seasonNumber, number)`.
fn episode_number(ep: &serde_json::Value) -> Option<(u32, u32)> {
    let n = |k: &str| {
        ep.get(k)
            .and_then(|x| x.as_u64())
            .and_then(|x| u32::try_from(x).ok())
    };
    Some((n("seasonNumber")?, n("number")?))
}

fn text_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(decode_entities)
}

fn missing_text(lang: &str) -> LocalizedText {
    LocalizedText {
        language: lang.to_string(),
        ..LocalizedText::default()
    }
}

/// Every language's text from a `/series/{id}/extended?meta=translations`
/// body (`data.translations.{name,overview}Translations[]`), one row per
/// language, the base-record language marked `is_original`. That language is
/// the translation TVDB flags `isPrimary`, else the series' `originalLanguage`;
/// its row carries the base record's own text.
pub(crate) fn parse_series_texts(body: &str) -> Vec<LocalizedText> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(data) = v.get("data") else {
        return Vec::new();
    };
    let mut rows: std::collections::BTreeMap<String, LocalizedText> = Default::default();
    let mut primary = None;
    let tr = data.get("translations");
    for (list, field) in [
        ("nameTranslations", "name"),
        ("overviewTranslations", "overview"),
    ] {
        let entries = tr
            .and_then(|t| t.get(list))
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten();
        for e in entries {
            if e.get("isAlias").and_then(|x| x.as_bool()) == Some(true) {
                continue;
            }
            let Some(lang) = e.get("language").and_then(|x| x.as_str()) else {
                continue;
            };
            let row = rows
                .entry(lang.to_string())
                .or_insert_with(|| missing_text(lang));
            let text = text_field(e, field);
            if field == "name" {
                row.title = row.title.take().or(text);
            } else {
                row.overview = row.overview.take().or(text);
            }
            if e.get("isPrimary").and_then(|x| x.as_bool()) == Some(true) {
                primary = Some(lang.to_string());
            }
        }
    }
    let original = primary.or_else(|| text_field(data, "originalLanguage"));
    if let Some(orig) = original {
        let row = rows
            .entry(orig.clone())
            .or_insert_with(|| missing_text(&orig));
        row.is_original = true;
        // The base record's own text wins: it is what enrichment stored, and
        // B564's replace pass recognises provider text by equality with it.
        row.title = text_field(data, "name").or(row.title.take());
        row.overview = text_field(data, "overview").or(row.overview.take());
    }
    rows.into_values()
        .filter(|r| r.title.is_some() || r.overview.is_some())
        .collect()
}

/// The rows of `all` for each of `languages`, plus the original, with a
/// no-text row for a language TVDB does not have. `seen` is told each wanted
/// language's outcome.
fn pick_languages(
    all: Vec<LocalizedText>,
    languages: &[String],
    mut seen: impl FnMut(&str, TranslationOutcome),
) -> Vec<LocalizedText> {
    let mut out: Vec<LocalizedText> = all
        .into_iter()
        .filter(|r| r.is_original || languages.contains(&r.language))
        .collect();
    for lang in languages {
        match out.iter().find(|r| &r.language == lang) {
            Some(r) if r.is_original => seen(lang, TranslationOutcome::Original),
            Some(r) if r.overview.is_some() || r.title.is_some() => {
                seen(lang, TranslationOutcome::Translated)
            }
            _ => {
                seen(lang, TranslationOutcome::Missing);
                out.push(missing_text(lang));
            }
        }
    }
    out
}

/// Put `lang`'s title and overview from `md.texts` into `md.title` /
/// `md.overview` where it has them; the base-record text stays otherwise.
pub(crate) fn prefer_language(md: &mut EnrichedMetadata, lang: &str) {
    let Some(row) = md.texts.iter().find(|r| r.language == lang) else {
        return;
    };
    if let Some(t) = &row.title {
        md.title = Some(t.clone());
    }
    if let Some(o) = &row.overview {
        md.overview = Some(o.clone());
    }
}

/// The languages one episode's text exists in, from the series episode list.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct EpisodeLanguages {
    /// The series' `originalLanguage` (`data.series`): TVDB writes base
    /// records in it.
    pub original: Option<String>,
    /// The episode's `nameTranslations` and `overviewTranslations` codes.
    pub listed: Vec<String>,
}

pub(crate) fn parse_episode_languages(body: &str, season: u32, episode: u32) -> EpisodeLanguages {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return EpisodeLanguages::default();
    };
    let data = v.get("data");
    let original = data
        .and_then(|d| d.get("series"))
        .and_then(|s| text_field(s, "originalLanguage"));
    let ep = data
        .and_then(|d| d.get("episodes"))
        .and_then(|e| e.as_array())
        .into_iter()
        .flatten()
        .find(|e| {
            e.get("seasonNumber").and_then(|x| x.as_u64()) == Some(u64::from(season))
                && e.get("number").and_then(|x| x.as_u64()) == Some(u64::from(episode))
        });
    let mut listed: Vec<String> = ["nameTranslations", "overviewTranslations"]
        .iter()
        .filter_map(|k| ep.and_then(|e| e.get(*k)).and_then(|l| l.as_array()))
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_string))
        .collect();
    listed.sort();
    listed.dedup();
    EpisodeLanguages { original, listed }
}

/// A `/episodes/{id}/translations/{lang}` body's text; `None` when it carries
/// neither a name nor an overview.
pub(crate) fn parse_translation(body: &str) -> Option<LocalizedText> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let data = v.get("data")?;
    let t = LocalizedText {
        language: text_field(data, "language").unwrap_or_default(),
        title: text_field(data, "name"),
        overview: text_field(data, "overview"),
        is_original: false,
    };
    (t.title.is_some() || t.overview.is_some()).then_some(t)
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
            // 004-books — no online metadata provider for books. TMDB and
            // TVDB index films and television; a book queried against either
            // returns a confident-looking wrong match, which is the failure
            // class that produced B150/B152/B159/B160 for album art. An online
            // book provider (Google Books / Comic Vine) is explicitly out of
            // scope for 004-books; book metadata comes from inside the file.
            MediaKind::Movie | MediaKind::Audio | MediaKind::Book => vec![],
        }
    }

    async fn fetch(
        &self,
        kind: MediaKind,
        id: &str,
        season: Option<u32>,
        episode: Option<u32>,
        name: Option<&str>,
    ) -> Option<EnrichedMetadata> {
        match kind {
            MediaKind::Episode => match (season, episode) {
                (Some(s), Some(e)) => self.0.get_episode(id, s, e, name).await,
                _ => self.0.get_series(id).await,
            },
            // 004-books — no online metadata provider for books. TMDB and
            // TVDB index films and television; a book queried against either
            // returns a confident-looking wrong match, which is the failure
            // class that produced B150/B152/B159/B160 for album art. An online
            // book provider (Google Books / Comic Vine) is explicitly out of
            // scope for 004-books; book metadata comes from inside the file.
            MediaKind::Movie | MediaKind::Audio | MediaKind::Book => None,
        }
    }

    async fn fetch_image_bytes(&self, url: &str) -> Option<Vec<u8>> {
        self.0.fetch_image_bytes(url).await
    }

    async fn list_images(
        &self,
        _kind: MediaKind,
        id: &str,
    ) -> Vec<crate::online_enrich::RemoteImage> {
        // TVDB only exposes series-level artwork; any `kind` maps to the
        // series id the caller resolved.
        self.0.list_series_artworks(id).await
    }

    async fn fetch_texts(
        &self,
        kind: MediaKind,
        id: &str,
        season: Option<u32>,
        episode: Option<u32>,
        name: Option<&str>,
        languages: &[String],
    ) -> TextsFetch {
        if kind != MediaKind::Episode {
            return TextsFetch::NotFound;
        }
        match (season, episode) {
            // A failed translation request fails the whole record, so the
            // backfill ages it a day and counts it toward its failure stop
            // instead of asking for it again on every pass.
            (Some(s), Some(e)) => match self.0.episode_record(id, s, e, name, languages).await {
                Ok((_, true)) => TextsFetch::Failed,
                Ok((md, false)) => TextsFetch::Found(md.texts),
                Err(miss) => miss,
            },
            _ => match self.0.series_record(id, languages).await {
                Ok(md) => TextsFetch::Found(md.texts),
                Err(miss) => miss,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn tvdb_parse_artworks_maps_known_types() {
        let body = r#"{"data":{"artworks":[
          {"image":"https://a/p.jpg","type":2,"language":"eng","score":100,"width":680,"height":1000},
          {"image":"https://a/bg.jpg","type":3,"language":null,"score":50},
          {"image":"https://a/logo.png","type":23,"language":"eng","score":10},
          {"image":"https://a/icon.jpg","type":5,"language":"eng","score":1}
        ]}}"#;
        let imgs = parse_tvdb_artworks(body);
        // types 2/3/23 kept (Primary/Backdrop/Logo); type 5 (icon) dropped.
        assert_eq!(imgs.len(), 3);
        let p = imgs
            .iter()
            .find(|i| i.role == ArtworkRole::Primary)
            .unwrap();
        assert_eq!(p.url, "https://a/p.jpg");
        assert_eq!(p.width, Some(680));
        assert_eq!(p.language.as_deref(), Some("eng"));
        assert!(imgs.iter().any(|i| i.role == ArtworkRole::Backdrop));
        assert!(imgs.iter().any(|i| i.role == ArtworkRole::Logo));
    }

    #[test]
    fn tvdb_parse_artworks_empty_on_garbage() {
        assert!(parse_tvdb_artworks("nope").is_empty());
    }

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
        paths: Arc<Mutex<Vec<String>>>,
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

        fn push_status(self, status: u16, body: &str) -> Self {
            self.gets
                .lock()
                .expect("lock")
                .push_back((status, body.to_string()));
            self
        }

        fn login_count(&self) -> usize {
            self.login_count.load(Ordering::SeqCst)
        }

        /// Every path requested, in order.
        fn paths(&self) -> Vec<String> {
            self.paths.lock().expect("lock").clone()
        }
    }

    impl TvdbTransport for FakeTransport {
        async fn get(&self, path: &str, _bearer: &str) -> (u16, String) {
            self.paths.lock().expect("lock").push(path.to_string());
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

    /// B557: trimmed from the live `/search?query=Dragon Ball&type=series`
    /// response (2026-11-05). The real shows are named in Japanese; the fan
    /// works ahead of them in TVDB's order are named in English.
    const DRAGON_BALL_SEARCH: &str = r#"{"data":[
      {"tvdb_id":"272910","name":"Dragon Ball Absalon","year":"2012","translations":{"eng":"Dragon Ball Absalon"},"aliases":[]},
      {"tvdb_id":"248580","name":"Dragon Ball Z Abridged","year":"2008","translations":{"eng":"Dragon Ball Z Abridged"},"aliases":["Dragon Ball Z: Abridged","DBZ: Abridged"]},
      {"tvdb_id":"374456","name":"Dragon Ball R&R","year":"2018","translations":{"eng":"Dragon Ball R&R"}},
      {"tvdb_id":"76666","name":"ドラゴンボール","year":"1986","translations":{"eng":"Dragon Ball","jpn":"ドラゴンボール"},"aliases":["Dragonball"]},
      {"tvdb_id":"347734","name":"スーパードラゴンボールヒーローズ","year":"2018","translations":{"eng":"Super Dragon Ball Heroes"}},
      {"tvdb_id":"81472","name":"ドラゴンボールZ","year":"1989","translations":{"eng":"Dragon Ball Z"},"aliases":["Dragonball Z","Dragon Ball Z (1989)"]}
    ]}"#;

    #[test]
    fn tvdb_series_search_matches_an_anime_by_its_english_translation() {
        let cands = super::parse_series_search(DRAGON_BALL_SEARCH);
        let real = cands.iter().find(|c| c.id == "76666").unwrap();
        assert_eq!(real.aliases, ["Dragon Ball", "Dragonball"]);
        let pick = |q| pharos_core::match_best(q, None, &cands, 0.7).unwrap().id;
        assert_eq!(pick("Dragon Ball"), "76666");
        assert_eq!(pick("Dragon Ball Z"), "81472");
    }

    #[test]
    fn decode_entities_decodes_tvdb_references_once() {
        assert_eq!(
            super::decode_entities("Dragon Ball R&amp;R"),
            "Dragon Ball R&R"
        );
        assert_eq!(
            super::decode_entities("&#39;85 &#x26; &quot;x&quot;"),
            "'85 & \"x\""
        );
        assert_eq!(super::decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(
            super::decode_entities("Cagney & Lacey; &bogus; a&"),
            "Cagney & Lacey; &bogus; a&"
        );
    }

    #[test]
    fn tvdb_series_detail_overview_has_no_html_references() {
        let body = r#"{"data":{"id":374456,"name":"Dragon Ball R&amp;R",
            "overview":"Dragon Ball R&amp;R is a fan-made audio manga drama"}}"#;
        let m = super::parse_series_detail(body).unwrap();
        assert_eq!(m.title.as_deref(), Some("Dragon Ball R&R"));
        assert_eq!(
            m.overview.as_deref(),
            Some("Dragon Ball R&R is a fan-made audio manga drama")
        );
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

    /// B125 — a show must be able to acquire a BACKDROP. `data.image` is the
    /// poster and nothing else, so every show in the deployed library (0 of
    /// 178) had one, and its detail page fell back to a video frame. The same
    /// `/series/{id}/extended` body already carries `artworks[]`.
    #[test]
    fn series_detail_takes_backdrop_and_logo_from_artworks() {
        let body = r#"{"data":{"name":"Death Note","image":"https://art/poster.jpg",
            "artworks":[
              {"type":3,"image":"https://art/fanart-low.jpg","score":10},
              {"type":3,"image":"https://art/fanart-best.jpg","score":99},
              {"type":23,"image":"https://art/logo.png","score":50},
              {"type":2,"image":"https://art/other-poster.jpg","score":80}
            ]}}"#;
        let e = super::parse_series_detail(body).expect("parses");
        let of = |r: ArtworkRole| {
            e.artwork
                .iter()
                .find(|a| a.role == r)
                .map(|a| a.url.clone())
        };
        assert_eq!(
            of(ArtworkRole::Backdrop).as_deref(),
            Some("https://art/fanart-best.jpg"),
            "the highest-scored background wins, not the first listed"
        );
        assert_eq!(
            of(ArtworkRole::Logo).as_deref(),
            Some("https://art/logo.png")
        );
        assert_eq!(
            of(ArtworkRole::Primary).as_deref(),
            Some("https://art/poster.jpg"),
            "`data.image` stays authoritative for the poster"
        );
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
        let e = super::parse_episode_detail(body, 1, 1, None).unwrap();
        assert_eq!(e.title.as_deref(), Some("Winter Is Coming"));
        assert_eq!(e.overview.as_deref(), Some("Lord Stark..."));
        assert_eq!(e.provider_id.as_deref(), Some("1"));
        // B125 — an episode still is its Primary, not its Thumb.
        assert!(e
            .artwork
            .iter()
            .any(|a| a.role == ArtworkRole::Primary && a.url.ends_with("ep1.jpg")));
        // aired "2011-04-17" -> unix seconds at UTC midnight.
        assert_eq!(e.premiere_date, Some(1_302_998_400));

        let e2 = super::parse_episode_detail(body, 1, 2, None).unwrap();
        // aired "2011-04-24" -> unix seconds at UTC midnight.
        assert_eq!(e2.premiere_date, Some(1_303_603_200));
    }

    /// TVDB's season of The Next Generation, which now counts the two-part
    /// pilot once: every later episode sits one number below the files'.
    const TNG_S1: &str = r#"{"data":{"episodes":[
        {"id":101,"seasonNumber":1,"number":1,"name":"Encounter at Farpoint"},
        {"id":102,"seasonNumber":1,"number":2,"name":"The Naked Now","overview":"An infection."},
        {"id":103,"seasonNumber":1,"number":3,"name":"Code of Honor","overview":"Tasha Yar is kidnapped."},
        {"id":104,"seasonNumber":1,"number":4,"name":"The Last Outpost"},
        {"id":108,"seasonNumber":1,"number":8,"name":"Pilot"},
        {"id":201,"seasonNumber":2,"number":1,"name":"Graduation Day (1)"},
        {"id":202,"seasonNumber":2,"number":2,"name":"Graduation Day (2)"},
        {"id":209,"seasonNumber":2,"number":9,"name":"Twin"},
        {"id":210,"seasonNumber":2,"number":10,"name":"Twin"}
    ]}}"#;

    /// B570 — an episode named in its file takes the record of that name,
    /// one number off, not the one its number points at.
    #[test]
    fn an_episode_takes_the_record_its_name_matches() {
        let e = super::parse_episode_detail(TNG_S1, 1, 3, Some("The Naked Now")).unwrap();
        assert_eq!(e.title.as_deref(), Some("The Naked Now"));
        assert_eq!(e.overview.as_deref(), Some("An infection."));
        assert_eq!(e.provider_id.as_deref(), Some("102"));
        assert_eq!(e.episode_number, Some((1, 2)));
    }

    /// B570 — the numbered record stays when its name agrees, when the
    /// episode's name is a stand-in or matches nothing, when the match is
    /// further than a renumbering shifts, in another season, a different
    /// part, or a tie.
    #[test]
    fn an_episode_keeps_its_numbered_record_without_a_clear_named_one() {
        let pick = |season, episode, name| {
            super::parse_episode_detail(TNG_S1, season, episode, name).and_then(|e| e.provider_id)
        };
        assert_eq!(pick(1, 3, Some("Code of Honour")).as_deref(), Some("103"));
        assert_eq!(pick(1, 3, None).as_deref(), Some("103"));
        assert_eq!(pick(1, 3, Some("Episode 2")).as_deref(), Some("103"));
        assert_eq!(pick(1, 3, Some("hdtv")).as_deref(), Some("103"));
        assert_eq!(pick(1, 3, Some("33")).as_deref(), Some("103"));
        assert_eq!(pick(1, 3, Some("Datalore")).as_deref(), Some("103"));
        assert_eq!(
            pick(1, 3, Some("Pilot")).as_deref(),
            Some("103"),
            "five away"
        );
        assert_eq!(
            pick(2, 1, Some("Encounter at Farpoint")).as_deref(),
            Some("201")
        );
        assert_eq!(
            pick(2, 1, Some("Graduation Day (1)")).as_deref(),
            Some("201")
        );
        assert_eq!(
            pick(2, 3, Some("Graduation Day (2)")).as_deref(),
            Some("202")
        );
        assert_eq!(
            pick(2, 1, Some("Graduation Day (3)")).as_deref(),
            Some("201")
        );
        assert_eq!(
            pick(2, 8, Some("Twin")).as_deref(),
            None,
            "a tie keeps the (absent) number"
        );
        assert_eq!(
            super::parse_episode_detail(TNG_S1, 1, 3, None)
                .unwrap()
                .episode_number,
            Some((1, 3))
        );
    }

    /// B570 — an episode the list has no record for at its number is still
    /// found by its name.
    #[test]
    fn an_episode_past_the_end_of_the_list_is_found_by_its_name() {
        let e = super::parse_episode_detail(TNG_S1, 1, 5, Some("The Last Outpost")).unwrap();
        assert_eq!(e.provider_id.as_deref(), Some("104"));
    }

    #[test]
    fn tvdb_parse_episode_detail_none_when_no_match() {
        let body = r#"{"data":{"episodes":[{"id":1,"seasonNumber":1,"number":1,"name":"Winter Is Coming"}]}}"#;
        assert!(super::parse_episode_detail(body, 9, 9, None).is_none());
    }

    #[test]
    fn tvdb_parse_episode_detail_none_on_malformed_body() {
        assert!(super::parse_episode_detail("not json", 1, 1, None).is_none());
        assert!(super::parse_episode_detail("{}", 1, 1, None).is_none());
    }

    /// A Japanese base record with an English translation inline.
    const ANIME_SERIES: &str = r#"{"data":{"id":76666,"name":"ドラゴンボール",
        "overview":"悟空の冒険。","originalLanguage":"jpn",
        "translations":{
          "nameTranslations":[
            {"language":"jpn","name":"ドラゴンボール","isPrimary":true},
            {"language":"eng","name":"Dragon Ball"},
            {"language":"eng","name":"DB","isAlias":true}],
          "overviewTranslations":[
            {"language":"jpn","overview":"悟空の冒険。","isPrimary":true},
            {"language":"eng","overview":"Goku sets out.","isPrimary":false},
            {"language":"fra","overview":"Goku part."}]}}}"#;

    fn by_lang<'a>(texts: &'a [LocalizedText], lang: &str) -> Option<&'a LocalizedText> {
        texts.iter().find(|t| t.language == lang)
    }

    #[tokio::test]
    async fn series_text_is_english_when_tvdb_has_it() {
        let t = FakeTransport::new().push_login("jwt").push_ok(ANIME_SERIES);
        let c = TvdbClient::with_transport("key".into(), t.clone());
        let md = c.get_series("76666").await.unwrap();
        assert_eq!(t.paths(), vec!["/series/76666/extended?meta=translations"]);
        assert_eq!(md.overview.as_deref(), Some("Goku sets out."));
        assert_eq!(md.title.as_deref(), Some("Dragon Ball"));
        let orig = by_lang(&md.texts, "jpn").expect("original kept");
        assert!(orig.is_original);
        assert_eq!(orig.overview.as_deref(), Some("悟空の冒険。"));
        let eng = by_lang(&md.texts, "eng").expect("english row");
        assert!(!eng.is_original);
        assert!(
            by_lang(&md.texts, "fra").is_none(),
            "only asked-for languages are kept"
        );
    }

    #[tokio::test]
    async fn series_without_english_keeps_the_original_and_records_the_miss() {
        let body = r#"{"data":{"id":1,"name":"ドラゴンボール","overview":"悟空の冒険。",
            "originalLanguage":"jpn","translations":{"overviewTranslations":[
            {"language":"jpn","overview":"悟空の冒険。","isPrimary":true}]}}}"#;
        let t = FakeTransport::new().push_login("jwt").push_ok(body);
        let c = TvdbClient::with_transport("key".into(), t);
        let md = c.get_series("1").await.unwrap();
        assert_eq!(md.overview.as_deref(), Some("悟空の冒険。"));
        let eng = by_lang(&md.texts, "eng").expect("a miss is recorded");
        assert_eq!(
            (eng.title.as_deref(), eng.overview.as_deref()),
            (None, None)
        );
    }

    #[test]
    fn series_texts_fall_back_to_original_language_and_base_text() {
        let body = r#"{"data":{"name":"Les Revenants","overview":"Un village.",
            "originalLanguage":"fra"}}"#;
        let texts = super::parse_series_texts(body);
        assert_eq!(
            texts,
            vec![LocalizedText {
                language: "fra".into(),
                title: Some("Les Revenants".into()),
                overview: Some("Un village.".into()),
                is_original: true,
            }]
        );
    }

    const ANIME_EPISODES: &str = r#"{"data":{"series":{"id":76666,"originalLanguage":"jpn"},
        "episodes":[{"id":9,"seasonNumber":1,"number":1,"name":"ブルマと孫悟空",
        "overview":"ブルマが…","nameTranslations":["jpn","eng"],
        "overviewTranslations":["jpn","eng"]},
        {"id":10,"seasonNumber":1,"number":2,"name":"えっ!","overview":"…",
        "nameTranslations":["jpn"],"overviewTranslations":["jpn"]}]}}"#;

    #[tokio::test]
    async fn episode_text_is_english_from_its_translation() {
        let t = FakeTransport::new()
            .push_login("jwt")
            .push_ok(ANIME_EPISODES)
            .push_ok(r#"{"data":{"language":"eng","name":"The Secret of the Dragon Balls","overview":"Bulma meets Goku."}}"#);
        let c = TvdbClient::with_transport("key".into(), t.clone());
        let md = c.get_episode("76666", 1, 1, None).await.unwrap();
        assert_eq!(
            t.paths(),
            vec![
                "/series/76666/episodes/default",
                "/episodes/9/translations/eng"
            ]
        );
        assert_eq!(md.title.as_deref(), Some("The Secret of the Dragon Balls"));
        assert_eq!(md.overview.as_deref(), Some("Bulma meets Goku."));
        let orig = by_lang(&md.texts, "jpn").expect("original kept");
        assert!(orig.is_original);
        assert_eq!(orig.title.as_deref(), Some("ブルマと孫悟空"));
    }

    /// B570 — the episode's name places it for its texts too: the
    /// translation is asked of the record it names, not the numbered one.
    #[tokio::test]
    async fn an_episodes_texts_come_from_the_record_its_name_places() {
        use crate::online_enrich::OnlineEnricher;
        let body = r#"{"data":{"series":{"originalLanguage":"eng"},"episodes":[
            {"id":102,"seasonNumber":1,"number":2,"name":"The Naked Now","nameTranslations":["eng","fra"]},
            {"id":103,"seasonNumber":1,"number":3,"name":"Code of Honor","nameTranslations":["eng"]}
        ]}}"#;
        let t = FakeTransport::new()
            .push_login("jwt")
            .push_ok(body)
            .push_ok(r#"{"data":{"language":"fra","name":"Le Virus"}}"#);
        let got = TvdbEnricher(TvdbClient::with_transport("key".into(), t.clone()))
            .fetch_texts(
                MediaKind::Episode,
                "71470",
                Some(1),
                Some(3),
                Some("The Naked Now"),
                &["fra".into()],
            )
            .await;
        assert_eq!(
            t.paths(),
            vec![
                "/series/71470/episodes/default",
                "/episodes/102/translations/fra"
            ]
        );
        let TextsFetch::Found(texts) = got else {
            panic!("found: {got:?}");
        };
        assert_eq!(
            by_lang(&texts, "fra")
                .and_then(|r| r.title.clone())
                .as_deref(),
            Some("Le Virus")
        );
    }

    #[tokio::test]
    async fn episode_not_listing_english_costs_no_request() {
        let t = FakeTransport::new()
            .push_login("jwt")
            .push_ok(ANIME_EPISODES);
        let c = TvdbClient::with_transport("key".into(), t.clone());
        let md = c.get_episode("76666", 1, 2, None).await.unwrap();
        assert_eq!(t.paths(), vec!["/series/76666/episodes/default"]);
        assert_eq!(md.title.as_deref(), Some("えっ!"));
        let eng = by_lang(&md.texts, "eng").expect("a miss is recorded");
        assert_eq!(eng.overview, None);
    }

    #[tokio::test]
    async fn episode_translation_404_is_a_miss_and_a_failure_is_not_recorded() {
        let t = FakeTransport::new()
            .push_login("jwt")
            .push_ok(ANIME_EPISODES)
            .push_status(404, "");
        let c = TvdbClient::with_transport("key".into(), t);
        let md = c.get_episode("76666", 1, 1, None).await.unwrap();
        assert_eq!(md.overview.as_deref(), Some("ブルマが…"));
        assert!(by_lang(&md.texts, "eng").is_some_and(|r| r.overview.is_none()));

        let t = FakeTransport::new()
            .push_login("jwt")
            .push_ok(ANIME_EPISODES)
            .push_status(500, "");
        let c = TvdbClient::with_transport("key".into(), t);
        let md = c.get_episode("76666", 1, 1, None).await.unwrap();
        assert_eq!(md.overview.as_deref(), Some("ブルマが…"));
        assert!(
            by_lang(&md.texts, "eng").is_none(),
            "a failed request leaves no row, so it is asked again"
        );
    }

    #[tokio::test]
    async fn english_show_needs_no_translation_request() {
        let body = r#"{"data":{"series":{"originalLanguage":"eng"},"episodes":[
            {"id":1,"seasonNumber":1,"number":1,"name":"Winter Is Coming",
             "overview":"Lord Stark...","overviewTranslations":["eng","fra"]}]}}"#;
        let t = FakeTransport::new().push_login("jwt").push_ok(body);
        let c = TvdbClient::with_transport("key".into(), t.clone());
        let md = c.get_episode("121361", 1, 1, None).await.unwrap();
        assert_eq!(t.paths().len(), 1);
        assert_eq!(md.overview.as_deref(), Some("Lord Stark..."));
        assert_eq!(md.texts.len(), 1);
        assert!(md.texts[0].is_original && md.texts[0].language == "eng");
    }

    /// The signal (B564): each language decision is counted. Fails when the
    /// client stops recording.
    #[tokio::test]
    async fn translation_decisions_are_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let t = FakeTransport::new()
            .push_login("jwt")
            .push_ok(ANIME_EPISODES)
            .push_ok(r#"{"data":{"name":"A","overview":"B"}}"#)
            .push_ok(ANIME_EPISODES)
            .push_ok(ANIME_SERIES);
        let c = TvdbClient::with_transport("key".into(), t);
        c.get_episode("76666", 1, 1, None).await.unwrap();
        c.get_episode("76666", 1, 2, None).await.unwrap();
        c.get_series("76666").await.unwrap();
        let mut seen = Vec::new();
        for (ck, _, _, v) in snapshotter.snapshot().into_vec() {
            if ck.key().name() != "pharos_metadata_translation_total" {
                continue;
            }
            let outcome = ck
                .key()
                .labels()
                .find(|l| l.key() == "outcome")
                .map(|l| l.value().to_string())
                .unwrap_or_default();
            if let DebugValue::Counter(n) = v {
                seen.push((outcome, n));
            }
        }
        seen.sort();
        assert_eq!(
            seen,
            vec![("missing".to_string(), 1), ("translated".to_string(), 2)]
        );
    }

    /// A failed translation request makes `fetch_texts` fail, so the backfill
    /// does not take the record as done and ask again on every pass.
    #[tokio::test]
    async fn a_failed_translation_request_fails_the_texts_fetch() {
        use crate::online_enrich::OnlineEnricher;
        let fetch = |status: u16| async move {
            let t = FakeTransport::new()
                .push_login("jwt")
                .push_ok(ANIME_EPISODES)
                .push_status(status, "");
            TvdbEnricher(TvdbClient::with_transport("key".into(), t))
                .fetch_texts(
                    MediaKind::Episode,
                    "76666",
                    Some(1),
                    Some(1),
                    None,
                    &["eng".into()],
                )
                .await
        };
        assert_eq!(fetch(500).await, TextsFetch::Failed);
        let TextsFetch::Found(missed) = fetch(404).await else {
            panic!("a 404 is an answer");
        };
        assert!(by_lang(&missed, "eng").is_some_and(|r| r.overview.is_none()));
    }

    /// An episode the series' list does not hold, or a series TVDB answers
    /// 404 for, is `NotFound`; a failed list request is `Failed`.
    #[tokio::test]
    async fn a_record_tvdb_does_not_have_is_not_a_failure() {
        use crate::online_enrich::OnlineEnricher;
        let episode = |t: FakeTransport, season: u32| async move {
            TvdbEnricher(TvdbClient::with_transport("key".into(), t))
                .fetch_texts(
                    MediaKind::Episode,
                    "76666",
                    Some(season),
                    Some(1),
                    None,
                    &["eng".into()],
                )
                .await
        };
        let list = || {
            FakeTransport::new()
                .push_login("jwt")
                .push_ok(ANIME_EPISODES)
        };
        assert_eq!(episode(list(), 99).await, TextsFetch::NotFound);
        let failed = FakeTransport::new().push_login("jwt").push_status(500, "");
        assert_eq!(episode(failed, 1).await, TextsFetch::Failed);
        // A 200 whose body is not a list (cut off in transit) failed too.
        let cut = FakeTransport::new().push_login("jwt").push_ok("");
        assert_eq!(episode(cut, 1).await, TextsFetch::Failed);
        let series = FakeTransport::new().push_login("jwt").push_status(404, "");
        let got = TvdbEnricher(TvdbClient::with_transport("key".into(), series))
            .fetch_texts(
                MediaKind::Episode,
                "76666",
                None,
                None,
                None,
                &["eng".into()],
            )
            .await;
        assert_eq!(got, TextsFetch::NotFound);
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
