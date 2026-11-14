//! A book's subjects from Open Library (ADR-0129, spec 043).
//!
//! Asked only for a book whose own file gave no usable subject. Two rungs:
//!
//! 1. By ISBN: `/isbn/{isbn}.json` names the edition's works; the first
//!    work's `/works/{id}.json` carries `subjects`.
//! 2. By title and author: `/search.json`, five results at most. A result is
//!    used only when its normalised title equals the book's and one of its
//!    author names shares a word with the book's author. A wrong shelf looks
//!    deliberate, so anything less is a miss.
//!
//! Only the title, author and ISBN leave the server, under a User-Agent with
//! no contact address ([`USER_AGENT`]). Requests are at most one per
//! [`MIN_INTERVAL`] across the process; a 429 or 503 backs off and retries
//! once.
//!
//! The HTTP layer is the [`Fetch`] trait so the rungs, the match rule, the
//! retry and the error paths are tested with a fake and never touch the
//! network.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;

/// Open Library's base URL.
const BASE: &str = "https://openlibrary.org";

/// The gap between two requests. Open Library asks a client that makes many
/// requests to keep to about one a second; the extra 100 ms keeps clock jitter
/// from putting two in the same second.
pub const MIN_INTERVAL: Duration = Duration::from_millis(1100);

/// The wait after a 429 or 503 with no `Retry-After`.
pub const THROTTLE_BACKOFF: Duration = Duration::from_secs(5);

/// Results asked of a search. The match rule rejects most; past five the
/// rest are rarely the same book.
const SEARCH_LIMIT: &str = "5";

/// Shortest author word that counts towards an overlap. Initials and "de",
/// "von" match too many people.
const MIN_AUTHOR_WORD: usize = 3;

/// Sent on every request. Names the software and what it is for, and no
/// person: no contact address, no host name.
pub fn user_agent() -> String {
    format!(
        "pharos/{} (self-hosted media server; book subjects)",
        env!("CARGO_PKG_VERSION")
    )
}

/// What one GET came back with.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    /// The `Retry-After` header in seconds, when the server sent one.
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

/// One HTTP GET. `Err` is a transport failure, carrying its cause.
pub trait Fetch: Send + Sync {
    fn get(
        &self,
        url: &str,
    ) -> impl std::future::Future<Output = Result<FetchResponse, String>> + Send;
}

/// The real [`Fetch`]: reqwest with Pharos's TLS settings and [`user_agent`].
#[derive(Clone)]
pub struct HttpFetch {
    http: reqwest::Client,
}

impl HttpFetch {
    pub fn new() -> Self {
        let http = crate::http_client::builder()
            .user_agent(user_agent())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            // Only TLS backend init can fail here; fall back rather than
            // panic (V17).
            .unwrap_or_else(|_| crate::http_client::client());
        Self { http }
    }
}

impl Default for HttpFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for HttpFetch {
    async fn get(&self, url: &str) -> Result<FetchResponse, String> {
        let resp = self.http.get(url).send().await.map_err(|e| {
            let mut cause = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                cause.push_str(": ");
                cause.push_str(&s.to_string());
                src = s.source();
            }
            cause
        })?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("reading the body: {e}"))?
            .to_vec();
        Ok(FetchResponse {
            status,
            retry_after,
            body,
        })
    }
}

/// What a book is looked up by. Only these three fields are ever sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookQuery {
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
}

impl BookQuery {
    /// The ISBN's digits (and a final `X`), when it has 10 or 13.
    pub fn isbn_digits(&self) -> Option<String> {
        let d: String = self
            .isbn
            .as_deref()?
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        matches!(d.len(), 10 | 13).then_some(d)
    }
}

/// Which rung found the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Isbn,
    Search,
}

impl Via {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Isbn => "isbn",
            Self::Search => "search",
        }
    }
}

/// A lookup's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The book was found; `subjects` may still be empty.
    Found {
        subjects: Vec<String>,
        via: Via,
        /// The work's key (`/works/OL…W`), for the log.
        work: String,
    },
    /// No rung found the book. `reason` names what was tried.
    NoMatch { reason: String },
}

/// Open Library could not be asked. Carries the URL path and the cause.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookupError {
    #[error("{what}: {cause}")]
    Transport { what: String, cause: String },
    #[error("{what} answered HTTP {status}")]
    Status { what: String, status: u16 },
    #[error("{what} answered HTTP {status} again after backing off")]
    Throttled { what: String, status: u16 },
    #[error("{what}: the answer did not decode: {cause}")]
    Decode { what: String, cause: String },
}

/// Serialises requests to at most one per interval. The next permitted
/// instant sits behind a mutex and each caller advances it, so concurrent
/// callers queue rather than fire together.
#[derive(Debug)]
struct RateGate {
    next: Mutex<Option<tokio::time::Instant>>,
    interval: Duration,
}

impl RateGate {
    async fn acquire(&self) {
        let mut next = self.next.lock().await;
        let now = tokio::time::Instant::now();
        let due = next.unwrap_or(now).max(now);
        if due > now {
            tokio::time::sleep_until(due).await;
        }
        *next = Some(due + self.interval);
    }

    /// Push every later request out by `extra`.
    async fn delay(&self, extra: Duration) {
        let mut next = self.next.lock().await;
        let now = tokio::time::Instant::now();
        *next = Some(next.unwrap_or(now).max(now) + extra);
    }
}

#[derive(Debug, Deserialize)]
struct Edition {
    #[serde(default)]
    works: Vec<KeyRef>,
    #[serde(default)]
    subjects: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct KeyRef {
    key: String,
}

#[derive(Debug, Deserialize)]
struct Work {
    #[serde(default)]
    subjects: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SearchAnswer {
    #[serde(default)]
    docs: Vec<SearchDoc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchDoc {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author_name: Vec<String>,
    #[serde(default)]
    pub subject: Vec<String>,
}

/// A title as the match rule compares it: the subtitle after `:` dropped,
/// lowercase, punctuation as spaces, whitespace collapsed.
pub fn normalised_title(title: &str) -> String {
    let main = title.split(':').next().unwrap_or(title);
    pharos_scanner::book::shelf::normalise(main)
}

fn author_words(name: &str) -> Vec<String> {
    pharos_scanner::book::shelf::normalise(name)
        .split(' ')
        .filter(|w| w.chars().count() >= MIN_AUTHOR_WORD)
        .map(str::to_owned)
        .collect()
}

/// Whether a search result is confidently the book: equal normalised titles
/// and an author word in common. A book with no author never matches a
/// search, since a title alone names too many books.
pub fn is_confident_match(query: &BookQuery, doc: &SearchDoc) -> bool {
    let want = normalised_title(&query.title);
    if want.is_empty() || normalised_title(&doc.title) != want {
        return false;
    }
    let Some(author) = query.author.as_deref() else {
        return false;
    };
    let mine = author_words(author);
    doc.author_name
        .iter()
        .flat_map(|a| author_words(a))
        .any(|w| mine.contains(&w))
}

/// A work key that is safe to put in a URL: `/works/` and an id of letters
/// and digits.
fn valid_work_key(key: &str) -> bool {
    key.strip_prefix("/works/")
        .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// The Open Library client. Cheap to clone: the gate is shared.
#[derive(Clone)]
pub struct OpenLibrary<F> {
    fetch: Arc<F>,
    gate: Arc<RateGate>,
    backoff: Duration,
    base: String,
}

impl OpenLibrary<HttpFetch> {
    pub fn new() -> Self {
        Self::with_timing(HttpFetch::new(), MIN_INTERVAL, THROTTLE_BACKOFF)
    }
}

impl Default for OpenLibrary<HttpFetch> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Fetch> OpenLibrary<F> {
    /// A client over `fetch` with the given request gap and throttle backoff.
    pub fn with_timing(fetch: F, interval: Duration, backoff: Duration) -> Self {
        Self {
            fetch: Arc::new(fetch),
            gate: Arc::new(RateGate {
                next: Mutex::new(None),
                interval,
            }),
            backoff,
            base: BASE.to_owned(),
        }
    }

    /// GET `path` (with `query`) as JSON. `Ok(None)` is a 404.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<T>, LookupError> {
        let url = reqwest::Url::parse_with_params(&format!("{}{path}", self.base), query).map_err(
            |e| LookupError::Transport {
                what: path.to_owned(),
                cause: format!("bad url: {e}"),
            },
        )?;
        for attempt in 0..2 {
            self.gate.acquire().await;
            let resp =
                self.fetch
                    .get(url.as_str())
                    .await
                    .map_err(|cause| LookupError::Transport {
                        what: path.to_owned(),
                        cause,
                    })?;
            match resp.status {
                200..=299 => {
                    return serde_json::from_slice(&resp.body).map(Some).map_err(|e| {
                        LookupError::Decode {
                            what: path.to_owned(),
                            cause: e.to_string(),
                        }
                    })
                }
                404 => return Ok(None),
                429 | 503 if attempt == 0 => {
                    let wait = resp.retry_after.unwrap_or(self.backoff);
                    tracing::info!(
                        path,
                        status = resp.status,
                        backoff_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                        "open library: throttled, backing off before one retry"
                    );
                    self.gate.delay(wait).await;
                }
                429 | 503 => {
                    return Err(LookupError::Throttled {
                        what: path.to_owned(),
                        status: resp.status,
                    })
                }
                status => {
                    return Err(LookupError::Status {
                        what: path.to_owned(),
                        status,
                    })
                }
            }
        }
        Err(LookupError::Throttled {
            what: path.to_owned(),
            status: 0,
        })
    }

    /// The fake behind a test client.
    #[cfg(test)]
    pub(crate) fn fetch_for_tests(&self) -> &F {
        &self.fetch
    }

    /// The ISBN rung: `Ok(None)` when Open Library has no such edition or it
    /// names no usable work.
    async fn by_isbn(&self, isbn: &str) -> Result<Option<Lookup>, LookupError> {
        let Some(edition) = self
            .get_json::<Edition>(&format!("/isbn/{isbn}.json"), &[])
            .await?
        else {
            return Ok(None);
        };
        let Some(work) = edition
            .works
            .into_iter()
            .map(|w| w.key)
            .find(|k| valid_work_key(k))
        else {
            return Ok(None);
        };
        let subjects = match self.get_json::<Work>(&format!("{work}.json"), &[]).await? {
            Some(w) if !w.subjects.is_empty() => w.subjects,
            _ => edition.subjects,
        };
        Ok(Some(Lookup::Found {
            subjects,
            via: Via::Isbn,
            work,
        }))
    }

    /// The search rung, with the confident-match rule.
    async fn by_search(&self, q: &BookQuery) -> Result<Lookup, LookupError> {
        let Some(author) = q.author.as_deref().filter(|a| !a.trim().is_empty()) else {
            return Ok(Lookup::NoMatch {
                reason: "no author to confirm a title search".into(),
            });
        };
        let title = q.title.trim();
        let answer = self
            .get_json::<SearchAnswer>(
                "/search.json",
                &[
                    ("title", title),
                    ("author", author.trim()),
                    ("fields", "key,subject,title,author_name"),
                    ("limit", SEARCH_LIMIT),
                ],
            )
            .await?
            .unwrap_or(SearchAnswer { docs: Vec::new() });
        let seen = answer.docs.len();
        match answer
            .docs
            .into_iter()
            .find(|d| is_confident_match(q, d) && valid_work_key(&d.key))
        {
            Some(doc) => Ok(Lookup::Found {
                subjects: doc.subject,
                via: Via::Search,
                work: doc.key,
            }),
            None => Ok(Lookup::NoMatch {
                reason: format!("none of {seen} search results matched title and author"),
            }),
        }
    }

    /// Look a book up: by ISBN, then by title and author.
    pub async fn lookup(&self, q: &BookQuery) -> Result<Lookup, LookupError> {
        if let Some(isbn) = q.isbn_digits() {
            if let Some(found) = self.by_isbn(&isbn).await? {
                return Ok(found);
            }
        }
        self.by_search(q).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::VecDeque;

    type Queued = Vec<(String, VecDeque<Result<FetchResponse, String>>)>;

    /// Answers queued per URL path, in order; records every URL asked.
    #[derive(Default)]
    pub(crate) struct FakeFetch {
        pub(crate) answers: std::sync::Mutex<Queued>,
        pub(crate) asked: std::sync::Mutex<Vec<String>>,
    }

    pub(crate) fn ok(body: &str) -> Result<FetchResponse, String> {
        status(200, body)
    }

    pub(crate) fn status(code: u16, body: &str) -> Result<FetchResponse, String> {
        Ok(FetchResponse {
            status: code,
            retry_after: None,
            body: body.as_bytes().to_vec(),
        })
    }

    impl FakeFetch {
        pub(crate) fn on(self, path: &str, answer: Result<FetchResponse, String>) -> Self {
            {
                let mut a = self.answers.lock().unwrap();
                match a.iter_mut().find(|(p, _)| p == path) {
                    Some((_, q)) => q.push_back(answer),
                    None => a.push((path.to_owned(), VecDeque::from([answer]))),
                }
            }
            self
        }

        pub(crate) fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Fetch for FakeFetch {
        async fn get(&self, url: &str) -> Result<FetchResponse, String> {
            self.asked.lock().unwrap().push(url.to_owned());
            let path = url
                .strip_prefix(BASE)
                .unwrap_or(url)
                .split('?')
                .next()
                .unwrap_or_default()
                .to_owned();
            let mut a = self.answers.lock().unwrap();
            a.iter_mut()
                .find(|(p, _)| *p == path)
                .and_then(|(_, q)| q.pop_front())
                .unwrap_or_else(|| status(404, "{}"))
        }
    }

    pub(crate) fn client(fetch: FakeFetch) -> OpenLibrary<FakeFetch> {
        OpenLibrary::with_timing(fetch, Duration::ZERO, Duration::ZERO)
    }

    const EDITION: &str = r#"{"title":"Synthetic Book","works":[{"key":"/works/OL1W"}],
        "isbn_13":["9780000000002"]}"#;
    const WORK: &str = r#"{"title":"Synthetic Book","key":"/works/OL1W",
        "subjects":["Rust (Computer program language)","Accessible book"]}"#;
    const SEARCH: &str = r#"{"numFound":2,"docs":[
        {"key":"/works/OL9W","title":"Synthetic Book","author_name":["Other Person"],
         "subject":["Cooking"]},
        {"key":"/works/OL2W","title":"Synthetic Book: A Subtitle","author_name":["Ada Example"],
         "subject":["Machine learning","Python (Computer program language)"]}]}"#;

    fn query(isbn: Option<&str>) -> BookQuery {
        BookQuery {
            isbn: isbn.map(str::to_owned),
            title: "Synthetic Book".into(),
            author: Some("Example, Ada".into()),
        }
    }

    #[tokio::test]
    async fn an_isbn_finds_the_works_subjects() {
        let fetch = FakeFetch::default()
            .on("/isbn/9780000000002.json", ok(EDITION))
            .on("/works/OL1W.json", ok(WORK));
        let c = client(fetch);
        let got = c.lookup(&query(Some("978-0-00-000000-2"))).await.unwrap();
        assert_eq!(
            got,
            Lookup::Found {
                subjects: vec![
                    "Rust (Computer program language)".into(),
                    "Accessible book".into()
                ],
                via: Via::Isbn,
                work: "/works/OL1W".into(),
            }
        );
        let asked = c.fetch.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked.iter().all(|u| !u.contains("search")));
    }

    #[tokio::test]
    async fn an_unknown_isbn_falls_back_to_a_confident_search() {
        let fetch = FakeFetch::default().on("/search.json", ok(SEARCH));
        let c = client(fetch);
        let got = c.lookup(&query(Some("9780000000002"))).await.unwrap();
        let Lookup::Found {
            subjects,
            via,
            work,
        } = got
        else {
            panic!("expected a match, got {got:?}");
        };
        assert_eq!(via, Via::Search);
        assert_eq!(
            work, "/works/OL2W",
            "the same title by another author is not the book"
        );
        assert_eq!(
            subjects.first().map(String::as_str),
            Some("Machine learning")
        );
        let search = c
            .fetch
            .asked()
            .into_iter()
            .find(|u| u.contains("search"))
            .unwrap();
        assert!(search.contains("title=Synthetic+Book"), "{search}");
        assert!(search.contains("author=Example%2C+Ada"), "{search}");
        assert!(search.contains("limit=5"), "{search}");
    }

    #[tokio::test]
    async fn no_confident_result_is_a_miss() {
        let only_other = r#"{"docs":[{"key":"/works/OL9W","title":"Synthetic Book",
            "author_name":["Other Person"],"subject":["Cooking"]}]}"#;
        let c = client(FakeFetch::default().on("/search.json", ok(only_other)));
        assert!(matches!(
            c.lookup(&query(None)).await.unwrap(),
            Lookup::NoMatch { .. }
        ));
        let c = client(FakeFetch::default());
        let no_author = BookQuery {
            author: None,
            ..query(None)
        };
        assert!(matches!(
            c.lookup(&no_author).await.unwrap(),
            Lookup::NoMatch { .. }
        ));
        assert!(
            c.fetch.asked().is_empty(),
            "a book with no author is never searched"
        );
    }

    #[tokio::test]
    async fn a_throttle_backs_off_and_retries_once() {
        let fetch = FakeFetch::default()
            .on("/search.json", status(429, ""))
            .on("/search.json", ok(SEARCH));
        let c = client(fetch);
        assert!(matches!(
            c.lookup(&query(None)).await.unwrap(),
            Lookup::Found { .. }
        ));
        assert_eq!(c.fetch.asked().len(), 2);

        let fetch = FakeFetch::default()
            .on("/search.json", status(503, ""))
            .on("/search.json", status(503, ""))
            .on("/search.json", ok(SEARCH));
        let c = client(fetch);
        let err = c.lookup(&query(None)).await.unwrap_err();
        assert!(
            matches!(err, LookupError::Throttled { status: 503, .. }),
            "{err:?}"
        );
        assert_eq!(c.fetch.asked().len(), 2, "one retry, not more");
    }

    #[tokio::test]
    async fn http_and_transport_failures_name_the_cause() {
        let c = client(FakeFetch::default().on("/search.json", status(500, "")));
        let err = c.lookup(&query(None)).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "/search.json answered HTTP 500",
            "the log line must say which request and what it answered"
        );
        let c = client(FakeFetch::default().on("/search.json", Err("connection refused".into())));
        assert!(matches!(
            c.lookup(&query(None)).await.unwrap_err(),
            LookupError::Transport { .. }
        ));
        let c = client(FakeFetch::default().on("/search.json", ok("not json")));
        assert!(matches!(
            c.lookup(&query(None)).await.unwrap_err(),
            LookupError::Decode { .. }
        ));
    }

    #[test]
    fn the_match_rule_wants_title_and_author() {
        let doc = |title: &str, author: &str| SearchDoc {
            key: "/works/OL1W".into(),
            title: title.into(),
            author_name: vec![author.into()],
            subject: Vec::new(),
        };
        let q = query(None);
        assert!(is_confident_match(
            &q,
            &doc("Synthetic Book", "Ada Example")
        ));
        assert!(is_confident_match(
            &q,
            &doc("synthetic book!", "A. Example")
        ));
        assert!(is_confident_match(
            &q,
            &doc("Synthetic Book: Second Edition", "Ada Example")
        ));
        assert!(!is_confident_match(
            &q,
            &doc("Synthetic Books", "Ada Example")
        ));
        assert!(!is_confident_match(
            &q,
            &doc("Synthetic Book", "Bob Person")
        ));
        assert!(
            !is_confident_match(&q, &doc("Synthetic Book", "Al")),
            "a short word is no overlap"
        );
    }

    #[test]
    fn only_a_plain_work_key_is_followed() {
        assert!(valid_work_key("/works/OL123W"));
        assert!(!valid_work_key("/works/"));
        assert!(!valid_work_key("/authors/OL1A"));
        assert!(!valid_work_key("/works/OL1W/../../x"));
        assert!(!valid_work_key("https://elsewhere/works/OL1W"));
    }

    #[test]
    fn the_user_agent_names_no_person() {
        let ua = user_agent();
        assert!(ua.starts_with("pharos/"), "{ua}");
        assert!(!ua.contains('@'), "no email: {ua}");
        assert!(!ua.contains("http"), "no contact url: {ua}");
    }

    #[test]
    fn an_isbn_is_used_only_when_it_has_ten_or_thirteen_digits() {
        assert_eq!(
            query(Some("978-0-00-000000-2")).isbn_digits().as_deref(),
            Some("9780000000002")
        );
        assert_eq!(
            query(Some("000000000x")).isbn_digits().as_deref(),
            Some("000000000X")
        );
        assert_eq!(query(Some("B00SYNTH01")).isbn_digits(), None);
        assert_eq!(query(None).isbn_digits(), None);
    }
}
