//! HTTP client for the OpenSubtitles.com REST API v1 (spec 030, FR-002/FR-006).
//!
//! Every request carries `Api-Key` and `User-Agent`; `/download` carries a
//! bearer JWT when a username and password are configured. Requests are paced
//! to the documented 5/s limit with headroom, `Retry-After` is reported rather
//! than slept on (the caller decides), and 406 on `/download` closes the shared
//! [`QuotaGate`] until the reset time the API names.

use super::quota::{parse_utc_timestamp, QuotaGate, DEFAULT_PAUSE_SECS};
use super::{BoxFut, Downloaded, ProviderError, SearchQuery, SubtitleHit, SubtitleProvider};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_API_BASE: &str = "https://api.opensubtitles.com/api/v1";

/// Spacing between requests: 4/s against the documented 5/s per IP.
const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// A login token is valid for 24 h; renew an hour early.
const TOKEN_LIFETIME_SECS: i64 = 23 * 60 * 60;

/// Subtitle files are text; anything larger is not one.
const MAX_SUBTITLE_BYTES: usize = 4 * 1024 * 1024;

/// Pause suggested when a 429 carries no `Retry-After`.
const DEFAULT_RETRY_AFTER_SECS: u64 = 10;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Session {
    token: Option<String>,
    /// API root to use after login (`base_url` in the login response).
    base: Option<String>,
    expires_unix: i64,
}

pub struct OpenSubtitlesClient {
    http: reqwest::Client,
    api_key: String,
    login: Option<(String, String)>,
    api_base: String,
    user_agent: String,
    session: tokio::sync::Mutex<Session>,
    last_request: tokio::sync::Mutex<Option<Instant>>,
    min_interval: Duration,
    gate: Arc<QuotaGate>,
}

impl std::fmt::Debug for OpenSubtitlesClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSubtitlesClient")
            .field("api_base", &self.api_base)
            .field("has_login", &self.login.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct LoginResponse {
    token: Option<String>,
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    attributes: Attributes,
}

#[derive(Deserialize, Default)]
struct Attributes {
    #[serde(default)]
    subtitle_id: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    release: String,
    #[serde(default)]
    download_count: u32,
    #[serde(default)]
    ratings: f32,
    #[serde(default)]
    from_trusted: Option<bool>,
    #[serde(default)]
    hearing_impaired: bool,
    #[serde(default)]
    ai_translated: bool,
    #[serde(default)]
    machine_translated: bool,
    #[serde(default)]
    foreign_parts_only: bool,
    #[serde(default)]
    fps: Option<f32>,
    #[serde(default)]
    uploader: Option<Uploader>,
    #[serde(default)]
    upload_date: Option<String>,
    #[serde(default)]
    moviehash_match: bool,
    #[serde(default)]
    files: Vec<FileRef>,
}

#[derive(Deserialize)]
struct Uploader {
    name: Option<String>,
}

#[derive(Deserialize)]
struct FileRef {
    file_id: u64,
}

#[derive(Deserialize)]
struct DownloadResponse {
    link: Option<String>,
    file_name: Option<String>,
    remaining: Option<u32>,
    reset_time_utc: Option<String>,
}

#[derive(Deserialize)]
struct QuotaBody {
    reset_time_utc: Option<String>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Query parameters for `/subtitles`, lowercase and sorted by name as the API
/// documents for cacheability (V566).
pub fn search_params(q: &SearchQuery) -> Vec<(&'static str, String)> {
    let mut p: Vec<(&'static str, String)> = Vec::new();
    if let Some(v) = q.episode {
        p.push(("episode_number", v.to_string()));
    }
    if let Some(v) = q.imdb_id {
        p.push(("imdb_id", v.to_string()));
    }
    p.push(("languages", q.language.to_ascii_lowercase()));
    if let Some(v) = &q.moviehash {
        p.push(("moviehash", v.to_ascii_lowercase()));
    }
    if let Some(v) = q.parent_imdb_id {
        p.push(("parent_imdb_id", v.to_string()));
    }
    if let Some(v) = q.parent_tmdb_id {
        p.push(("parent_tmdb_id", v.to_string()));
    }
    if let Some(v) = q.season {
        p.push(("season_number", v.to_string()));
    }
    if let Some(v) = q.tmdb_id {
        p.push(("tmdb_id", v.to_string()));
    }
    p
}

impl OpenSubtitlesClient {
    pub fn new(
        api_key: String,
        login: Option<(String, String)>,
        gate: Arc<QuotaGate>,
    ) -> Result<Self, reqwest::Error> {
        let user_agent = format!("Pharos v{}", env!("CARGO_PKG_VERSION"));
        let http = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            api_key,
            login,
            api_base: DEFAULT_API_BASE.to_string(),
            user_agent,
            session: tokio::sync::Mutex::new(Session::default()),
            last_request: tokio::sync::Mutex::new(None),
            min_interval: MIN_INTERVAL,
            gate,
        })
    }

    /// Point the client at another API root (tests, a self-hosted mirror).
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_min_interval(mut self, d: Duration) -> Self {
        self.min_interval = d;
        self
    }

    async fn pace(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(next) = last.map(|t| t + self.min_interval) {
            let now = Instant::now();
            if next > now {
                tokio::time::sleep(next - now).await;
            }
        }
        *last = Some(Instant::now());
    }

    fn transport(op: &'static str) -> impl FnOnce(reqwest::Error) -> ProviderError {
        move |source| ProviderError::Transport { op, source }
    }

    fn retry_after(resp: &reqwest::Response) -> u64 {
        resp.headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
    }

    /// `(token, api root)` for the next call, logging in when needed. No
    /// configured credentials means no token.
    async fn session_for_call(&self) -> Result<(Option<String>, String), ProviderError> {
        let Some((user, pass)) = &self.login else {
            return Ok((None, self.api_base.clone()));
        };
        let mut s = self.session.lock().await;
        if let Some(token) = &s.token {
            if s.expires_unix > now_unix() {
                let base = s.base.clone().unwrap_or_else(|| self.api_base.clone());
                return Ok((Some(token.clone()), base));
            }
        }
        self.pace().await;
        let resp = self
            .http
            .post(format!("{}/login", self.api_base))
            .header("Api-Key", &self.api_key)
            .json(&serde_json::json!({ "username": user, "password": pass }))
            .send()
            .await
            .map_err(Self::transport("login"))?;
        let status = resp.status();
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                retry_after_secs: Self::retry_after(&resp),
            });
        }
        if !status.is_success() {
            return Err(ProviderError::Auth {
                status: status.as_u16(),
                message: "login refused".to_string(),
            });
        }
        let body: LoginResponse = resp.json().await.map_err(|e| ProviderError::Parse {
            op: "login",
            detail: e.to_string(),
        })?;
        let Some(token) = body.token.filter(|t| !t.is_empty()) else {
            return Err(ProviderError::Auth {
                status: status.as_u16(),
                message: "login response carried no token".to_string(),
            });
        };
        // The login host is reused only when it is a bare host name; the
        // configured API root stays authoritative otherwise (tests, mirrors).
        let base = body
            .base_url
            .filter(|h| !h.contains("://") && !h.contains('/') && self.api_base == DEFAULT_API_BASE)
            .map(|h| format!("https://{h}/api/v1"));
        s.token = Some(token.clone());
        s.base = base.clone();
        s.expires_unix = now_unix().saturating_add(TOKEN_LIFETIME_SECS);
        Ok((Some(token), base.unwrap_or_else(|| self.api_base.clone())))
    }

    async fn forget_token(&self) {
        self.session.lock().await.token = None;
    }

    async fn search_impl(&self, query: &SearchQuery) -> Result<Vec<SubtitleHit>, ProviderError> {
        let mut relogin = false;
        loop {
            let (token, base) = self.session_for_call().await?;
            self.pace().await;
            let mut req = self
                .http
                .get(format!("{base}/subtitles"))
                .header("Api-Key", &self.api_key)
                .query(&search_params(query));
            if let Some(t) = &token {
                req = req.bearer_auth(t);
            }
            let resp = req.send().await.map_err(Self::transport("search"))?;
            let status = resp.status().as_u16();
            match status {
                200 => {
                    let body: SearchResponse =
                        resp.json().await.map_err(|e| ProviderError::Parse {
                            op: "search",
                            detail: e.to_string(),
                        })?;
                    return Ok(body.data.into_iter().filter_map(hit_from).collect());
                }
                401 if token.is_some() && !relogin => {
                    relogin = true;
                    self.forget_token().await;
                }
                401 | 403 => {
                    return Err(ProviderError::Auth {
                        status,
                        message: "search refused".to_string(),
                    })
                }
                429 => {
                    return Err(ProviderError::RateLimited {
                        retry_after_secs: Self::retry_after(&resp),
                    })
                }
                _ => {
                    return Err(ProviderError::Http {
                        op: "search",
                        status,
                    })
                }
            }
        }
    }

    async fn download_impl(&self, file_id: u64) -> Result<Downloaded, ProviderError> {
        if self.gate.blocked_for(now_unix()).is_some() {
            return Err(ProviderError::QuotaExceeded {
                reset_unix: self.gate.blocked_until(),
            });
        }
        let mut relogin = false;
        let info: DownloadResponse = loop {
            let (token, base) = self.session_for_call().await?;
            self.pace().await;
            let mut req = self
                .http
                .post(format!("{base}/download"))
                .header("Api-Key", &self.api_key)
                .json(&serde_json::json!({ "file_id": file_id, "sub_format": "srt" }));
            if let Some(t) = &token {
                req = req.bearer_auth(t);
            }
            let resp = req.send().await.map_err(Self::transport("download"))?;
            let status = resp.status().as_u16();
            match status {
                200 => {
                    break resp.json().await.map_err(|e| ProviderError::Parse {
                        op: "download",
                        detail: e.to_string(),
                    })?
                }
                406 => {
                    let reset = resp
                        .json::<QuotaBody>()
                        .await
                        .ok()
                        .and_then(|b| b.reset_time_utc)
                        .and_then(|s| parse_utc_timestamp(&s))
                        .unwrap_or_else(|| now_unix().saturating_add(DEFAULT_PAUSE_SECS));
                    self.gate.block_until(reset);
                    return Err(ProviderError::QuotaExceeded { reset_unix: reset });
                }
                401 if token.is_some() && !relogin => {
                    relogin = true;
                    self.forget_token().await;
                }
                401 | 403 => {
                    return Err(ProviderError::Auth {
                        status,
                        message: "download refused".to_string(),
                    })
                }
                429 => {
                    return Err(ProviderError::RateLimited {
                        retry_after_secs: Self::retry_after(&resp),
                    })
                }
                _ => {
                    return Err(ProviderError::Http {
                        op: "download",
                        status,
                    })
                }
            }
        };
        if info.remaining == Some(0) {
            if let Some(reset) = info.reset_time_utc.as_deref().and_then(parse_utc_timestamp) {
                self.gate.block_until(reset);
            }
        }
        let Some(link) = info.link else {
            return Err(ProviderError::Parse {
                op: "download",
                detail: "response carried no link".to_string(),
            });
        };
        self.pace().await;
        let mut file = self
            .http
            .get(&link)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .send()
            .await
            .map_err(Self::transport("fetch"))?;
        let status = file.status().as_u16();
        if status != 200 {
            return Err(ProviderError::Http {
                op: "fetch",
                status,
            });
        }
        let too_big = |bytes: u64| ProviderError::Parse {
            op: "fetch",
            detail: format!("more than {MAX_SUBTITLE_BYTES} bytes ({bytes}) is not a subtitle"),
        };
        // Refuse a declared oversize body before reading any of it.
        if let Some(declared) = file.content_length() {
            if declared > MAX_SUBTITLE_BYTES as u64 {
                return Err(too_big(declared));
            }
        }
        // Read chunk by chunk and stop at the first chunk that crosses the cap,
        // so an undeclared (chunked) body is never buffered whole.
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = file.chunk().await.map_err(Self::transport("fetch"))? {
            let total = bytes.len().saturating_add(chunk.len());
            if total > MAX_SUBTITLE_BYTES {
                return Err(too_big(total as u64));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Downloaded {
            file_name: info.file_name.unwrap_or_default(),
            bytes,
            remaining: info.remaining,
        })
    }
}

fn hit_from(item: SearchItem) -> Option<SubtitleHit> {
    let a = item.attributes;
    let file_id = a.files.first()?.file_id;
    Some(SubtitleHit {
        file_id,
        subtitle_id: a.subtitle_id,
        language: a.language,
        release: a.release,
        download_count: a.download_count,
        ratings: a.ratings,
        from_trusted: a.from_trusted.unwrap_or(false),
        hearing_impaired: a.hearing_impaired,
        ai_translated: a.ai_translated,
        machine_translated: a.machine_translated,
        foreign_parts_only: a.foreign_parts_only,
        fps: a.fps,
        uploader: a.uploader.and_then(|u| u.name),
        upload_date: a.upload_date,
        hash_match: a.moviehash_match,
    })
}

impl SubtitleProvider for OpenSubtitlesClient {
    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFut<'a, Result<Vec<SubtitleHit>, ProviderError>> {
        Box::pin(self.search_impl(query))
    }

    fn download(&self, file_id: u64) -> BoxFut<'_, Result<Downloaded, ProviderError>> {
        Box::pin(self.download_impl(file_id))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use actix_web::{web, App, HttpRequest, HttpResponse};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Seen {
        /// `(method path, query, api-key, user-agent, authorization)`
        requests: Vec<(String, String, String, String, String)>,
        logins: u32,
        download_status: u16,
        /// First download answers 401 once when set.
        reject_first_token: bool,
        downloads: u32,
        remaining: u32,
        /// Point the download link at an oversized file instead of the small one.
        oversize: Option<Oversize>,
    }

    #[derive(Clone, Copy)]
    enum Oversize {
        /// `Content-Length` declared, body is `MAX_SUBTITLE_BYTES + 1` bytes.
        Declared,
        /// Chunked, no `Content-Length`, over the cap across many chunks.
        Chunked,
    }

    type Shared = web::Data<Mutex<Seen>>;

    fn header(req: &HttpRequest, name: &str) -> String {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    fn record(req: &HttpRequest, s: &mut Seen) {
        s.requests.push((
            format!("{} {}", req.method(), req.path()),
            req.query_string().to_string(),
            header(req, "api-key"),
            header(req, "user-agent"),
            header(req, "authorization"),
        ));
    }

    async fn login(req: HttpRequest, s: Shared) -> HttpResponse {
        let mut s = s.lock().unwrap();
        record(&req, &mut s);
        s.logins += 1;
        let n = s.logins;
        HttpResponse::Ok().json(serde_json::json!({
            "token": format!("jwt-{n}"),
            "base_url": "api.opensubtitles.com",
        }))
    }

    async fn subtitles(req: HttpRequest, s: Shared) -> HttpResponse {
        record(&req, &mut s.lock().unwrap());
        HttpResponse::Ok().json(serde_json::json!({"data": [
            {"id": "1", "type": "subtitle", "attributes": {
                "subtitle_id": "77", "language": "en", "release": "Movie.2020.1080p",
                "download_count": 1200, "ratings": 8.0, "from_trusted": true,
                "moviehash_match": true, "fps": 23.976,
                "uploader": {"name": "alice"}, "upload_date": "2020-05-01T10:00:00Z",
                "files": [{"file_id": 4242, "file_name": "a.srt"}]}},
            {"id": "2", "type": "subtitle", "attributes": {
                "subtitle_id": "78", "language": "en", "files": []}},
        ]}))
    }

    async fn download(req: HttpRequest, s: Shared) -> HttpResponse {
        let origin = format!("http://{}", req.connection_info().host());
        let mut s = s.lock().unwrap();
        record(&req, &mut s);
        s.downloads += 1;
        if s.reject_first_token && s.downloads == 1 {
            return HttpResponse::Unauthorized().finish();
        }
        match s.download_status {
            406 => HttpResponse::NotAcceptable().json(serde_json::json!({
                "message": "You have downloaded your allowed 20 subtitles for 24h.",
                "reset_time_utc": "2099-01-01T00:00:00.000Z",
            })),
            429 => HttpResponse::TooManyRequests()
                .insert_header(("Retry-After", "7"))
                .finish(),
            _ => HttpResponse::Ok().json(serde_json::json!({
                "link": match s.oversize {
                    None => format!("{origin}/file/9.srt"),
                    Some(Oversize::Declared) => format!("{origin}/huge/declared"),
                    Some(Oversize::Chunked) => format!("{origin}/huge/chunked"),
                },
                "file_name": "a.srt",
                "remaining": s.remaining,
                "reset_time_utc": "2099-01-01T00:00:00.000Z",
            })),
        }
    }

    async fn file() -> HttpResponse {
        HttpResponse::Ok().body("1\n00:00:01,000 --> 00:00:02,000\nhi\n")
    }

    async fn huge_declared() -> HttpResponse {
        HttpResponse::Ok().body(vec![b'x'; MAX_SUBTITLE_BYTES + 1])
    }

    /// 64 KiB chunks, no Content-Length: the client must cut it off mid-stream.
    async fn huge_chunked() -> HttpResponse {
        let chunks = (0..(MAX_SUBTITLE_BYTES / 65_536 + 8))
            .map(|_| Ok::<_, actix_web::Error>(web::Bytes::from(vec![b'x'; 65_536])));
        HttpResponse::Ok().streaming(futures_util::stream::iter(chunks.collect::<Vec<_>>()))
    }

    fn server(seen: Seen) -> (actix_test::TestServer, Shared) {
        let shared: Shared = web::Data::new(Mutex::new(seen));
        let data = shared.clone();
        let srv = actix_test::start(move || {
            App::new()
                .app_data(data.clone())
                .route("/login", web::post().to(login))
                .route("/subtitles", web::get().to(subtitles))
                .route("/download", web::post().to(download))
                .route("/file/{n}", web::get().to(file))
                .route("/huge/declared", web::get().to(huge_declared))
                .route("/huge/chunked", web::get().to(huge_chunked))
        });
        (srv, shared)
    }

    fn client(
        srv: &actix_test::TestServer,
        login: bool,
        gate: Arc<QuotaGate>,
    ) -> OpenSubtitlesClient {
        OpenSubtitlesClient::new(
            "key-123".to_string(),
            login.then(|| ("user".to_string(), "pw".to_string())),
            gate,
        )
        .unwrap()
        .with_api_base(srv.url("").trim_end_matches('/'))
        .with_min_interval(Duration::ZERO)
    }

    #[test]
    fn search_params_are_sorted_lowercase_and_omit_absent_ids() {
        let q = SearchQuery {
            moviehash: Some("ABCDEF0123456789".into()),
            imdb_id: Some(111161),
            tmdb_id: None,
            parent_imdb_id: Some(944947),
            parent_tmdb_id: Some(1399),
            season: Some(2),
            episode: Some(5),
            language: "EN".into(),
        };
        let p = search_params(&q);
        let names: Vec<&str> = p.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            [
                "episode_number",
                "imdb_id",
                "languages",
                "moviehash",
                "parent_imdb_id",
                "parent_tmdb_id",
                "season_number"
            ]
        );
        assert!(p.contains(&("moviehash", "abcdef0123456789".to_string())));
        assert!(p.contains(&("languages", "en".to_string())));
    }

    #[actix_web::test]
    async fn search_sends_key_agent_and_query_and_parses_hits() {
        let (srv, seen) = server(Seen::default());
        let c = client(&srv, false, Arc::new(QuotaGate::new()));
        let q = SearchQuery {
            moviehash: Some("8e245d9679d31e12".into()),
            imdb_id: Some(111161),
            language: "en".into(),
            ..Default::default()
        };
        let hits = c.search(&q).await.unwrap();
        assert_eq!(hits.len(), 1, "a result with no file is dropped");
        assert_eq!(hits[0].file_id, 4242);
        assert!(hits[0].hash_match && hits[0].from_trusted);
        assert_eq!(hits[0].uploader.as_deref(), Some("alice"));
        let s = seen.lock().unwrap();
        let (route, query, key, agent, auth) = &s.requests[0];
        assert_eq!(route, "GET /subtitles");
        assert_eq!(
            query,
            "imdb_id=111161&languages=en&moviehash=8e245d9679d31e12"
        );
        assert_eq!(key, "key-123");
        assert!(agent.starts_with("Pharos v"), "{agent}");
        assert_eq!(auth, "");
        assert_eq!(s.logins, 0, "search with no credentials never logs in");
    }

    #[actix_web::test]
    async fn download_logs_in_once_reuses_the_token_and_fetches_the_file() {
        let (srv, seen) = server(Seen {
            download_status: 200,
            remaining: 19,
            ..Default::default()
        });
        let c = client(&srv, true, Arc::new(QuotaGate::new()));
        let first = c.download(4242).await.unwrap();
        let second = c.download(4243).await.unwrap();
        assert_eq!(first.remaining, Some(19));
        assert_eq!(first.file_name, "a.srt");
        assert!(String::from_utf8(first.bytes)
            .unwrap()
            .contains("00:00:01,000"));
        assert_eq!(second.file_name, "a.srt");
        let s = seen.lock().unwrap();
        assert_eq!(s.logins, 1);
        let auths: Vec<&str> = s
            .requests
            .iter()
            .filter(|r| r.0 == "POST /download")
            .map(|r| r.4.as_str())
            .collect();
        assert_eq!(auths, ["Bearer jwt-1", "Bearer jwt-1"]);
    }

    /// V563-adjacent: an oversized subtitle is refused, declared or not, and
    /// never comes back as bytes.
    #[actix_web::test]
    async fn an_oversized_subtitle_is_refused_with_or_without_content_length() {
        for oversize in [Oversize::Declared, Oversize::Chunked] {
            let (srv, _seen) = server(Seen {
                download_status: 200,
                oversize: Some(oversize),
                ..Default::default()
            });
            let c = client(&srv, true, Arc::new(QuotaGate::new()));
            match c.download(1).await {
                Err(ProviderError::Parse {
                    op: "fetch",
                    detail,
                }) => {
                    assert!(detail.contains("is not a subtitle"), "{detail}");
                }
                other => panic!("expected a refused fetch, got {other:?}"),
            }
        }
    }

    #[actix_web::test]
    async fn a_401_with_a_token_logs_in_again_once() {
        let (srv, seen) = server(Seen {
            download_status: 200,
            reject_first_token: true,
            ..Default::default()
        });
        let c = client(&srv, true, Arc::new(QuotaGate::new()));
        c.download(1).await.unwrap();
        let s = seen.lock().unwrap();
        assert_eq!(s.logins, 2);
        let auths: Vec<&str> = s
            .requests
            .iter()
            .filter(|r| r.0 == "POST /download")
            .map(|r| r.4.as_str())
            .collect();
        assert_eq!(auths, ["Bearer jwt-1", "Bearer jwt-2"]);
    }

    #[actix_web::test]
    async fn quota_406_closes_the_gate_and_blocks_the_next_call_without_a_request() {
        let (srv, seen) = server(Seen {
            download_status: 406,
            ..Default::default()
        });
        let gate = Arc::new(QuotaGate::new());
        let c = client(&srv, true, gate.clone());
        let err = c.download(1).await.unwrap_err();
        let reset = parse_utc_timestamp("2099-01-01T00:00:00Z").unwrap();
        assert!(matches!(err, ProviderError::QuotaExceeded { reset_unix } if reset_unix == reset));
        assert_eq!(gate.blocked_until(), reset);
        let before = seen.lock().unwrap().requests.len();
        let again = c.download(2).await.unwrap_err();
        assert!(matches!(again, ProviderError::QuotaExceeded { .. }));
        assert_eq!(
            seen.lock().unwrap().requests.len(),
            before,
            "no request while the gate is closed"
        );
    }

    #[actix_web::test]
    async fn rate_limit_429_reports_retry_after_and_leaves_the_gate_open() {
        let (srv, _seen) = server(Seen {
            download_status: 429,
            ..Default::default()
        });
        let gate = Arc::new(QuotaGate::new());
        let c = client(&srv, true, gate.clone());
        let err = c.download(1).await.unwrap_err();
        assert!(matches!(
            err,
            ProviderError::RateLimited {
                retry_after_secs: 7
            }
        ));
        assert_eq!(gate.blocked_for(now_unix()), None);
    }

    #[test]
    fn debug_output_hides_the_key_and_credentials() {
        let c = OpenSubtitlesClient::new(
            "super-secret-key".into(),
            Some(("alice".into(), "hunter2".into())),
            Arc::new(QuotaGate::new()),
        )
        .unwrap();
        let d = format!("{c:?}");
        assert!(
            !d.contains("super-secret-key") && !d.contains("hunter2") && !d.contains("alice"),
            "{d}"
        );
    }
}
