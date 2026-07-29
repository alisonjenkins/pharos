//! Album artwork from MusicBrainz + the Cover Art Archive.
//!
//! Most of this library's music files carry no embedded `attached_pic` stream
//! and no `cover.jpg` sidecar, so `has_primary_art` is honestly `false` and
//! every album, album-artist and track tile falls back to a flat icon. TMDB and
//! TVDB cover film and television and have nothing to say about music, which is
//! why the audio arm of the metadata backfill has always recorded `skipped`.
//!
//! MusicBrainz fills that gap and needs **no API key**: the release-group
//! search is public, and the Cover Art Archive serves the front cover for a
//! release-group id at a stable URL. In exchange it asks for two things, both
//! honoured here:
//!
//!   * a descriptive `User-Agent` naming the application and a contact URL —
//!     an anonymous or browser-impersonating agent is explicitly blocked; and
//!   * **at most one request per second** across the whole application
//!     ([`RateGate`]). This is not advisory: exceeding it earns a 503 and then
//!     an IP block.
//!
//! Scope is deliberately artwork-only. MusicBrainz models an *album* while
//! pharos stores a row per *track*, so the lookup is keyed on
//! `(album artist, album)` and memoised — a twelve-track album costs one search
//! and one cover download, not twelve of each.
//!
//! One cover lights up four views. A cached Primary flips `has_primary_art`,
//! and the album, album-artist and artist tiles all synthesise their image
//! from a representative track, so covering the tracks covers everything above
//! them. What this does NOT get you is a *photograph of the artist*: the Cover
//! Art Archive holds release art exclusively, and every provider carrying
//! artist portraits requires a key. An artist tile shows one of their album
//! covers — a real image rather than a blank card, but not the artist.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pharos_core::{ArtworkRole, SearchCandidate};
use tokio::sync::Mutex;

use crate::online_enrich::RemoteArt;

/// MusicBrainz web-service base.
const MB_BASE: &str = "https://musicbrainz.org/ws/2";

/// Cover Art Archive base. Redirects to `archive.org` storage, so the HTTP
/// client must follow redirects (reqwest does by default).
const CAA_BASE: &str = "https://coverartarchive.org";

/// The minimum gap between two MusicBrainz requests. Their published limit is
/// one request per second per application; we ask for slightly more headroom so
/// clock jitter can't put two calls inside the same second.
const MB_MIN_INTERVAL: Duration = Duration::from_millis(1100);

/// Confidence a release-group title must reach before its cover is accepted.
/// Album titles are short and collide freely ("Greatest Hits", "Home"), so this
/// sits above the film/TV default — a wrong cover is worse than no cover,
/// because it looks deliberate.
const MIN_ALBUM_CONFIDENCE: f32 = 0.85;

/// Bumped whenever the query LADDER changes in a way that could turn a past
/// miss into a hit.
///
/// A miss is stamped so the album's other eleven tracks don't each re-run a
/// rate-limited search, and the TTL then holds that verdict for 30 days. That
/// is right for a stable query and wrong the moment the query improves: the
/// first live pass recorded 22 `no_candidates` and B142 fixed most of them,
/// but every one of those rows was already stamped and would have waited until
/// late August to benefit. Stamping the version alongside the miss lets the
/// eligibility query re-admit exactly the rows whose verdict was reached by an
/// older, worse query — without disturbing albums that genuinely have no
/// artwork under the current one.
///
/// v1 — initial ladder: exact `(album artist, album)` only.
/// v2 — B142: edition qualifiers stripped, compilation placeholders dropped,
///      title-only last resort.
/// v3 — B144: unquoted final rung, so a tag truncated mid-word can still find
///      its release-group. INEFFECTIVE — see v4.
/// v4 — B145: the unquoted rung wildcards its final token. MusicBrainz's
///      search ANDs bare terms, so an unquoted `... Outgu` still required the
///      clipped token to match something and returned nothing; `Outgu*` is
///      what a truncated tag actually needs.
pub const ALBUM_ART_QUERY_VERSION: u32 = 4;

/// The `match_external_id` written beside a miss, carrying the query version
/// that reached it. Distinguishable from a hit (a release-group MBID) by shape,
/// so nothing can confuse the two.
pub fn miss_marker() -> String {
    format!("miss-v{ALBUM_ART_QUERY_VERSION}")
}

/// How long to wait after a 503 when the server names no `Retry-After`.
const THROTTLE_BACKOFF: Duration = Duration::from_secs(5);

/// Shortest final token that may carry a trailing wildcard. Below this a
/// wildcard matches most of the database, and every candidate it returns is
/// noise for the similarity floor to reject.
const MIN_WILDCARD_STEM: usize = 4;

/// How many release-group candidates to consider per search.
const SEARCH_LIMIT: u32 = 8;

/// `(album artist, album)` → the matched release-group id, or `None` when the
/// search already came back empty.
type AlbumMemo = Arc<Mutex<HashMap<(String, String), Option<String>>>>;

/// The most recently downloaded cover, keyed by release-group id.
type CoverSlot = Arc<Mutex<Option<(String, Vec<u8>)>>>;

/// Serialises outbound MusicBrainz calls to at most one per
/// [`MB_MIN_INTERVAL`].
///
/// A plain "sleep before each call" is not enough under concurrency: two tasks
/// would sleep in parallel and then fire together. The gate holds the *next
/// permitted instant* behind a mutex and each caller advances it, so waiters
/// queue instead of bunching.
#[derive(Debug)]
struct RateGate(Mutex<Option<tokio::time::Instant>>);

impl RateGate {
    fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Push the next permitted instant out by `extra`, so every waiter behind
    /// this one backs off too. Called when the server says it is being asked
    /// too often — otherwise the next queued album walks straight into the same
    /// 503.
    async fn delay(&self, extra: Duration) {
        let mut next = self.0.lock().await;
        let base = next.unwrap_or_else(tokio::time::Instant::now);
        *next = Some(base.max(tokio::time::Instant::now()) + extra);
    }

    /// Block until this caller's slot is due, then claim the next one.
    async fn acquire(&self) {
        let mut next = self.0.lock().await;
        let now = tokio::time::Instant::now();
        let due = next.unwrap_or(now).max(now);
        if due > now {
            tokio::time::sleep_until(due).await;
        }
        *next = Some(due + MB_MIN_INTERVAL);
    }
}

/// Why an album lookup produced no artwork. Carries the offending value rather
/// than a bare class, so the log line answers "why is this album still blank?"
/// without a second investigation.
#[derive(Debug, Clone, PartialEq)]
pub enum AlbumArtMiss {
    /// The track has no `album` tag, so there is nothing to search for.
    NoAlbumTag,
    /// MusicBrainz returned no release-group candidates for the query.
    NoCandidates { query: String },
    /// Candidates came back but none reached [`MIN_ALBUM_CONFIDENCE`].
    BelowConfidence { best: String, confidence: f32 },
    /// A release-group matched but the Cover Art Archive has no front cover
    /// for it — common for bootlegs and small labels.
    NoCoverArt { mbid: String },
    /// Transport / HTTP / decode failure. Carries the underlying cause.
    Unavailable { cause: String },
}

impl AlbumArtMiss {
    /// Stable metric label. Bounded cardinality — the payloads stay in the log
    /// line, never in a label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NoAlbumTag => "no_album_tag",
            Self::NoCandidates { .. } => "no_candidates",
            Self::BelowConfidence { .. } => "below_confidence",
            Self::NoCoverArt { .. } => "no_cover_art",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

impl std::fmt::Display for AlbumArtMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAlbumTag => write!(f, "track carries no album tag"),
            Self::NoCandidates { query } => {
                write!(f, "musicbrainz returned no release-group for `{query}`")
            }
            Self::BelowConfidence { best, confidence } => write!(
                f,
                "best release-group `{best}` scored {confidence:.2}, below {MIN_ALBUM_CONFIDENCE:.2}"
            ),
            Self::NoCoverArt { mbid } => {
                write!(f, "cover art archive has no front cover for {mbid}")
            }
            Self::Unavailable { cause } => write!(f, "provider unavailable: {cause}"),
        }
    }
}

/// A matched MusicBrainz release-group, and the cover bytes fetched for it.
#[derive(Debug, Clone)]
pub struct AlbumArt {
    /// The MusicBrainz release-group id — persisted so a re-run skips the
    /// search.
    pub mbid: String,
    /// The release-group title as MusicBrainz spells it.
    pub title: String,
    /// Confidence that won the match.
    pub confidence: f32,
    /// First release year, when MusicBrainz states one.
    pub year: Option<u32>,
    /// The front-cover bytes.
    pub bytes: Vec<u8>,
}

impl AlbumArt {
    /// The artwork this cover stands in for. Album covers are Primary art —
    /// the same role a `cover.jpg` sidecar would fill.
    pub fn remote_art(&self) -> RemoteArt {
        RemoteArt {
            role: ArtworkRole::Primary,
            url: front_cover_url(&self.mbid),
        }
    }
}

/// The Cover Art Archive front-cover URL for a release-group id.
fn front_cover_url(mbid: &str) -> String {
    format!("{CAA_BASE}/release-group/{mbid}/front")
}

/// Cache key for an album lookup: the album artist (lowercased, empty when
/// unknown) and the album title (lowercased). Two tracks of the same album
/// share one entry regardless of tag casing.
fn album_key(artist: Option<&str>, album: &str) -> (String, String) {
    (
        artist.unwrap_or_default().to_lowercase(),
        album.to_lowercase(),
    )
}

/// Escape a value for a Lucene field query. MusicBrainz's search index is
/// Lucene, so an unescaped `"` or `\` in an album title silently changes the
/// query's meaning (or makes it a syntax error) — "Quotation Marks" and
/// `AC\DC`-style tags are real.
fn lucene_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '"'
                | '+'
                | '-'
                | '!'
                | '('
                | ')'
                | ':'
                | '^'
                | '['
                | ']'
                | '{'
                | '}'
                | '~'
                | '*'
                | '?'
                | '|'
                | '&'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build the release-group search query for an album, narrowed by artist when
/// the file carries one. Without an artist the album title alone has to carry
/// the match, which is why [`MIN_ALBUM_CONFIDENCE`] is set where it is.
pub(crate) fn release_group_query(artist: Option<&str>, album: &str) -> String {
    match artist.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!(
            "releasegroup:\"{}\" AND artist:\"{}\"",
            lucene_escape(album),
            lucene_escape(a)
        ),
        None => format!("releasegroup:\"{}\"", lucene_escape(album)),
    }
}

/// The `album_artist` values that name a compilation rather than a performer.
///
/// MusicBrainz credits a compilation to its actual contributors, so searching
/// `artist:"Various Artists"` excludes the very release you are looking for.
/// Measured on the first live pass: every `Ministry of Sound` compilation
/// missed for exactly this reason.
fn is_compilation_artist(artist: &str) -> bool {
    matches!(
        artist.trim().to_ascii_lowercase().as_str(),
        "various artists" | "various" | "va" | "soundtrack" | "original soundtrack"
    )
}

/// Strip the edition/format qualifiers tags carry and MusicBrainz does not
/// index into the release-group title.
///
/// A release GROUP is the abstract album; the pressing details — `(Japan)`,
/// `[Bonus Tracks]`, `(CD EP UK & Europe)`, `(CDr Promo) US` — belong to an
/// individual release, so a title carrying them matches no release-group at
/// all. Measured on the first live pass: of 22 `no_candidates` misses, most
/// were a real album wearing one of these suffixes.
///
/// Only TRAILING bracketed groups are removed, and only when something is left
/// — a title that is entirely bracketed keeps its brackets rather than becoming
/// the empty string, and an interior bracket (`Sign "O" the Times (Disc 1)` vs
/// `Where (Are We)? Now`) is untouched.
pub(crate) fn strip_edition_qualifiers(album: &str) -> String {
    let mut out = album.trim();
    loop {
        let trimmed = out.trim_end();
        // A trailing bare region/format token, e.g. `... (CDr Promo) US`.
        let after_word = match trimmed.rsplit_once(char::is_whitespace) {
            Some((head, last))
                if last.len() <= 3
                    && !last.is_empty()
                    && last.chars().all(|c| c.is_ascii_uppercase())
                    && head.ends_with([')', ']']) =>
            {
                head.trim_end()
            }
            _ => trimmed,
        };
        let Some(open) = after_word
            .strip_suffix(')')
            .map(|_| '(')
            .or_else(|| after_word.strip_suffix(']').map(|_| '['))
        else {
            out = after_word;
            break;
        };
        let Some(idx) = after_word.rfind(open) else {
            out = after_word;
            break;
        };
        let head = after_word[..idx].trim_end();
        if head.is_empty() {
            // The whole title is bracketed — stripping it would leave nothing
            // to search for, which is worse than searching the odd title.
            out = after_word;
            break;
        }
        out = head;
    }
    out.to_string()
}

/// The ordered search attempts for one album, most specific first.
///
/// Each attempt costs a rate-limited request, so the ladder is only walked
/// while an attempt returns NO candidates at all — a wrong-but-present result
/// is a confidence question, not a query question, and re-asking would not
/// improve it. The vec never contains duplicates: an album with no qualifiers
/// and a normal artist yields exactly one attempt, which is the common case.
pub(crate) fn search_attempts(artist: Option<&str>, album: &str) -> Vec<String> {
    let artist = artist.map(str::trim).filter(|a| !a.is_empty());
    // A compilation is credited to its contributors on MusicBrainz, so naming
    // "Various Artists" as the artist excludes the release we want.
    let effective_artist = artist.filter(|a| !is_compilation_artist(a));
    let stripped = strip_edition_qualifiers(album);

    let mut out = vec![release_group_query(effective_artist, album)];
    let mut push = |q: String| {
        if !out.contains(&q) {
            out.push(q);
        }
    };
    if stripped != album {
        push(release_group_query(effective_artist, &stripped));
    }
    // Last resort: the title alone. A tag whose artist field is wrong (a
    // per-track performer on a compilation, a mis-parsed "Artist - Date"
    // string) still names its album correctly.
    if effective_artist.is_some() {
        push(release_group_query(None, &stripped));
    }
    // Final rung: the same words UNQUOTED. Every rung above is a quoted phrase,
    // which Lucene matches exactly — so a tag truncated mid-word finds nothing
    // at all, however close it is. `Always Outnumbered Never Outgu` (a real tag
    // in this library, clipped by whatever wrote it) misses every quoted query
    // and would score 0.88 against `Always Outnumbered, Never Outgunned` if the
    // search ever returned it. Unquoted, Lucene scores partial matches and does.
    //
    // This rung is only safe because acceptance is still gated on
    // `MIN_ALBUM_CONFIDENCE` — a loose query returns loose candidates, and the
    // similarity floor is what stops one becoming a wrong cover.
    if let Some(q) = unquoted_query(&stripped) {
        push(q);
    }
    out
}

/// An unquoted Lucene release-group query — the words, not the phrase, with a
/// trailing wildcard on the last one.
///
/// B145: the wildcard is the whole point, and B144 shipped without it. Bare
/// terms are ANDed by MusicBrainz's search, so an unquoted
/// `Always Outnumbered Never Outgu` STILL required the clipped token `Outgu`
/// to match a real word and still returned nothing — the rung was inert for
/// the exact case it was written for. `Outgu*` matches `Outgunned`, which is
/// what a truncated tag needs. The similarity floor then decides, as before.
///
/// `None` when the title has nothing to search on once Lucene's operators are
/// removed, which would otherwise produce a query matching everything.
fn unquoted_query(album: &str) -> Option<String> {
    // Drop the characters Lucene reads as syntax rather than escaping them: a
    // bare `-` is NOT, a bare `:` opens a field, and an unbalanced bracket is a
    // parse error. Apostrophes stay INSIDE a word — splitting on them turned
    // `Prospekt's March` into the two terms `Prospekt s`, which ANDed against a
    // stray `s` and matched nothing.
    let words: Vec<&str> = album
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .collect();
    let (last, head) = words.split_last()?;
    let mut terms: Vec<String> = head.iter().map(|w| (*w).to_string()).collect();
    // Only a word long enough to be distinctive gets the wildcard — `a*` or
    // `II*` would match most of the database and hand the similarity floor a
    // pile of noise to reject.
    if last.len() >= MIN_WILDCARD_STEM {
        terms.push(format!("{last}*"));
    } else {
        terms.push((*last).to_string());
    }
    Some(format!("releasegroup:({})", terms.join(" ")))
}

/// The `Retry-After` delay a throttling response asks for, when it sends one.
///
/// Only the delta-seconds form is honoured — the HTTP-date form would need a
/// clock comparison to be meaningful, and MusicBrainz sends seconds. Absurd
/// values are clamped so a malformed header cannot park the backfill for a day.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.clamp(1, 60)))
}

/// Render an error and every cause beneath it.
///
/// `reqwest`'s `Display` stops at "error sending request for url (...)", which
/// names the request and not the failure — the first live pass logged six of
/// those and none said whether it was DNS, TLS, a timeout or a refused
/// connection. Walking `source()` puts the actual cause in the log line.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        src = cause.source();
    }
    out
}

/// Parse a MusicBrainz `/release-group` search body into ranked candidates.
///
/// Pure (no I/O) so the JSON shape is unit-tested without touching the network
/// — MusicBrainz has no sandbox and its rate limit makes live tests hostile.
/// Malformed JSON or a missing `release-groups` array yields an empty `Vec`
/// rather than panicking: a provider blip must never fail a scan (V6).
pub(crate) fn parse_release_groups(body: &str) -> Vec<SearchCandidate> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let Some(groups) = v.get("release-groups").and_then(|g| g.as_array()) else {
        return vec![];
    };
    groups
        .iter()
        .filter_map(|g| {
            Some(SearchCandidate {
                id: g.get("id")?.as_str()?.to_string(),
                title: g.get("title")?.as_str()?.to_string(),
                year: g
                    .get("first-release-date")
                    .and_then(|d| d.as_str())
                    .and_then(|d| d.get(0..4))
                    .and_then(|y| y.parse().ok()),
            })
        })
        .collect()
}

/// Whether a Cover Art Archive response body is plausibly an image.
///
/// The archive answers a missing cover with a 404 whose body is JSON, and a
/// redirect chain can land on an HTML error page while still reporting 200.
/// Writing either into the image cache would put a broken tile on every album
/// in the library, so the bytes are sniffed before they are accepted. Checks
/// the magic numbers for the three formats the archive actually serves.
pub(crate) fn looks_like_image(bytes: &[u8]) -> bool {
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G'];
    const GIF: &[u8] = b"GIF8";
    bytes.starts_with(JPEG) || bytes.starts_with(PNG) || bytes.starts_with(GIF)
}

/// Resolves a track's album cover.
///
/// Abstracted for the same reason [`crate::tmdb::PersonImageResolver`] is: the
/// backfill pass that drives it must be testable against a deterministic fake,
/// and MusicBrainz has no sandbox — its rate limit makes a live test in CI
/// actively hostile to the service.
pub trait AlbumArtResolver: Send + Sync {
    /// Resolve and download the front cover for `(artist, album)`.
    fn album_art(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
    ) -> impl std::future::Future<Output = Result<AlbumArt, AlbumArtMiss>> + Send;
}

impl AlbumArtResolver for MusicBrainzClient {
    async fn album_art(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
    ) -> Result<AlbumArt, AlbumArtMiss> {
        MusicBrainzClient::album_art(self, artist, album).await
    }
}

/// MusicBrainz + Cover Art Archive album-art resolver.
///
/// Cheap to clone (shares the connection pool, the rate gate and the memo), so
/// one instance is built at boot and handed to every backfill task.
#[derive(Clone)]
pub struct MusicBrainzClient {
    http: reqwest::Client,
    gate: Arc<RateGate>,
    /// `(artist, album)` → the matched release-group id, or `None` when the
    /// search already came back empty. Memoising the misses matters as much as
    /// the hits: without it every track of an unmatched album re-runs a
    /// rate-limited search.
    memo: AlbumMemo,
    /// The most recently downloaded cover, keyed by release-group id. Album
    /// tracks arrive together, so a single slot turns a twelve-track album into
    /// one download without holding the whole library's artwork in memory.
    last_cover: CoverSlot,
}

impl MusicBrainzClient {
    /// Build a client. `contact` is the URL or address MusicBrainz should use
    /// to reach the operator if this instance misbehaves — it goes in the
    /// `User-Agent`, which their policy requires to be identifying.
    pub fn new(contact: &str) -> Self {
        let ua = format!(
            "pharos/{} ( {} )",
            env!("CARGO_PKG_VERSION"),
            contact.trim()
        );
        let http = reqwest::Client::builder()
            .user_agent(ua)
            .timeout(Duration::from_secs(20))
            .build()
            // `ClientBuilder::build` only fails on TLS backend init, which
            // would already have failed for TMDB; fall back rather than panic
            // (V17 forbids `expect` here anyway).
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            gate: Arc::new(RateGate::new()),
            memo: Arc::new(Mutex::new(HashMap::new())),
            last_cover: Arc::new(Mutex::new(None)),
        }
    }

    /// Resolve the release-group id for `(artist, album)`, consulting the memo
    /// first. `Err` carries why nothing was found.
    async fn release_group(
        &self,
        artist: Option<&str>,
        album: &str,
    ) -> Result<(String, Option<u32>, f32), AlbumArtMiss> {
        let key = album_key(artist, album);
        if let Some(hit) = self.memo.lock().await.get(&key) {
            return match hit {
                // A memoised hit has already cleared the confidence bar; the
                // score is not re-reported because the search is not re-run.
                Some(mbid) => Ok((mbid.clone(), None, MIN_ALBUM_CONFIDENCE)),
                None => Err(AlbumArtMiss::NoCandidates {
                    query: search_attempts(artist, album).join(" | "),
                }),
            };
        }

        // Most specific first. The ladder is only walked while an attempt
        // returns NO candidates — a wrong-but-present result is a confidence
        // question, not a query question.
        let attempts = search_attempts(artist, album);
        let mut candidates = Vec::new();
        let mut query = attempts
            .first()
            .cloned()
            .unwrap_or_else(|| release_group_query(artist, album));
        for (n, attempt) in attempts.iter().enumerate() {
            let found = self.search_release_groups(attempt).await?;
            if !found.is_empty() {
                if n > 0 {
                    tracing::debug!(
                        album,
                        attempt = n,
                        query = attempt.as_str(),
                        "musicbrainz: a relaxed query found candidates the exact one did not"
                    );
                }
                query = attempt.clone();
                candidates = found;
                break;
            }
        }
        // Score against the stripped title: the qualifiers that were removed to
        // find the release-group would otherwise count against its similarity.
        let scored_title = strip_edition_qualifiers(album);
        let best = pharos_core::match_best(&scored_title, None, &candidates, MIN_ALBUM_CONFIDENCE);

        let outcome = match best {
            Some(m) => m,
            None => {
                // Remember the miss so the album's other tracks don't each
                // spend a rate-limited search rediscovering it.
                self.memo.lock().await.insert(key, None);
                return Err(match candidates.first() {
                    Some(c) => AlbumArtMiss::BelowConfidence {
                        best: c.title.clone(),
                        confidence: pharos_core::match_best(&scored_title, None, &candidates, 0.0)
                            .map(|m| m.confidence)
                            .unwrap_or(0.0),
                    },
                    None => AlbumArtMiss::NoCandidates { query },
                });
            }
        };

        let year = candidates
            .iter()
            .find(|c| c.id == outcome.id)
            .and_then(|c| c.year);
        self.memo.lock().await.insert(key, Some(outcome.id.clone()));
        Ok((outcome.id, year, outcome.confidence))
    }

    /// One rate-limited release-group search, retried once on a throttle.
    ///
    /// B145: the first passes logged `release-group search returned 503`, which
    /// is MusicBrainz saying "slow down" — their limiter is a burst budget, not
    /// a flat one-per-second, and the B142 ladder turned one request per album
    /// into up to four. Treating that as a plain failure both wasted the album
    /// and kept the pressure on. Backing off and retrying once is the polite
    /// response and the one their documentation asks for.
    async fn search_release_groups(
        &self,
        query: &str,
    ) -> Result<Vec<SearchCandidate>, AlbumArtMiss> {
        for attempt in 0..=1 {
            self.gate.acquire().await;
            let resp = self
                .http
                .get(format!("{MB_BASE}/release-group"))
                .query(&[
                    ("query", query),
                    ("fmt", "json"),
                    ("limit", &SEARCH_LIMIT.to_string()),
                ])
                .send()
                .await
                .map_err(|e| AlbumArtMiss::Unavailable {
                    cause: format!("release-group search: {}", error_chain(&e)),
                })?;
            let status = resp.status();
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE && attempt == 0 {
                let wait = retry_after(resp.headers()).unwrap_or(THROTTLE_BACKOFF);
                tracing::info!(
                    query,
                    backoff_ms = wait.as_millis() as u64,
                    "musicbrainz: throttled (503), backing off before one retry"
                );
                metrics::counter!("pharos_album_art_throttled_total").increment(1);
                // Push the shared gate out too, so every other album waits with
                // this one instead of walking straight into the same 503.
                self.gate.delay(wait).await;
                tokio::time::sleep(wait).await;
                continue;
            }
            if !status.is_success() {
                return Err(AlbumArtMiss::Unavailable {
                    cause: format!("release-group search returned {}", status.as_u16()),
                });
            }
            let body = resp.text().await.map_err(|e| AlbumArtMiss::Unavailable {
                cause: format!("reading release-group body: {}", error_chain(&e)),
            })?;
            return Ok(parse_release_groups(&body));
        }
        Err(AlbumArtMiss::Unavailable {
            cause: "release-group search still throttled (503) after one retry".into(),
        })
    }

    /// Download the front cover for a release-group, reusing the single-slot
    /// cache when the previous track belonged to the same album.
    async fn front_cover(&self, mbid: &str) -> Result<Vec<u8>, AlbumArtMiss> {
        if let Some((cached_mbid, bytes)) = self.last_cover.lock().await.as_ref() {
            if cached_mbid == mbid {
                return Ok(bytes.clone());
            }
        }
        // The Cover Art Archive is a separate service from the MusicBrainz web
        // service, but it is the same project and the same operator, so it
        // goes through the same gate.
        self.gate.acquire().await;
        let resp = self
            .http
            .get(front_cover_url(mbid))
            .send()
            .await
            .map_err(|e| AlbumArtMiss::Unavailable {
                cause: format!("cover art fetch: {}", error_chain(&e)),
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(AlbumArtMiss::NoCoverArt {
                mbid: mbid.to_string(),
            });
        }
        if !status.is_success() {
            return Err(AlbumArtMiss::Unavailable {
                cause: format!("cover art fetch returned {}", status.as_u16()),
            });
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AlbumArtMiss::Unavailable {
                cause: format!("reading cover art body: {}", error_chain(&e)),
            })?
            .to_vec();
        if !looks_like_image(&bytes) {
            // A JSON 404 body or an HTML error page relayed as 200 — writing
            // it into the cache would put a broken tile on the album forever.
            return Err(AlbumArtMiss::NoCoverArt {
                mbid: mbid.to_string(),
            });
        }
        *self.last_cover.lock().await = Some((mbid.to_string(), bytes.clone()));
        Ok(bytes)
    }

    /// Resolve and download the front cover for a track's album.
    ///
    /// `artist` should be the album artist where the file has one — a
    /// compilation's per-track artist would not match the release-group.
    pub async fn album_art(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
    ) -> Result<AlbumArt, AlbumArtMiss> {
        let album = album
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .ok_or(AlbumArtMiss::NoAlbumTag)?;
        let (mbid, year, confidence) = self.release_group(artist, album).await?;
        let bytes = self.front_cover(&mbid).await?;
        Ok(AlbumArt {
            mbid,
            title: album.to_string(),
            confidence,
            year,
            bytes,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn release_group_query_narrows_by_artist_when_present() {
        assert_eq!(
            release_group_query(Some("Owl City"), "Ocean Eyes"),
            "releasegroup:\"Ocean Eyes\" AND artist:\"Owl City\""
        );
        // A missing or blank artist tag must not emit an empty artist clause —
        // `artist:""` matches nothing, which would turn every untagged album
        // into a silent miss.
        assert_eq!(
            release_group_query(None, "Ocean Eyes"),
            "releasegroup:\"Ocean Eyes\""
        );
        assert_eq!(
            release_group_query(Some("   "), "Ocean Eyes"),
            "releasegroup:\"Ocean Eyes\""
        );
    }

    #[test]
    fn lucene_special_characters_are_escaped() {
        // Unescaped, the quote would close the field and the rest of the title
        // would parse as query syntax.
        assert_eq!(
            release_group_query(None, r#"Say "Hello""#),
            r#"releasegroup:"Say \"Hello\"""#
        );
        assert_eq!(
            release_group_query(Some(r"AC\DC"), "Back in Black"),
            r#"releasegroup:"Back in Black" AND artist:"AC\\DC""#
        );
        // A hyphen is Lucene's NOT operator; "Sgt. Pepper" style punctuation
        // must survive as literal text.
        assert!(release_group_query(None, "Post-Rock").contains(r"Post\-Rock"));
    }

    #[test]
    fn parse_release_groups_reads_id_title_and_year() {
        let body = r#"{
          "release-groups": [
            {"id": "abc-123", "title": "Ocean Eyes", "first-release-date": "2009-07-14"},
            {"id": "def-456", "title": "All Things Bright and Beautiful"}
          ]
        }"#;
        let got = parse_release_groups(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "abc-123");
        assert_eq!(got[0].title, "Ocean Eyes");
        assert_eq!(got[0].year, Some(2009));
        // A release-group with no date is still a usable candidate.
        assert_eq!(got[1].year, None);
    }

    #[test]
    fn parse_release_groups_degrades_on_malformed_bodies() {
        // A provider blip must never fail a scan (V6) — every one of these is
        // an empty result, not a panic.
        assert!(parse_release_groups("").is_empty());
        assert!(parse_release_groups("not json").is_empty());
        assert!(parse_release_groups("{}").is_empty());
        assert!(parse_release_groups(r#"{"release-groups": []}"#).is_empty());
        // Entries missing the fields we key on are dropped, not defaulted to
        // an empty id that would resolve to a 404 cover URL.
        assert!(parse_release_groups(r#"{"release-groups":[{"title":"x"}]}"#).is_empty());
        assert!(parse_release_groups(r#"{"release-groups":[{"id":"x"}]}"#).is_empty());
    }

    #[test]
    fn image_sniff_accepts_real_formats_and_rejects_error_bodies() {
        assert!(looks_like_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]));
        assert!(looks_like_image(&[0x89, b'P', b'N', b'G', 13, 10]));
        assert!(looks_like_image(b"GIF89a...."));
        // The two shapes that actually reach us on a miss.
        assert!(!looks_like_image(br#"{"error":"Not Found"}"#));
        assert!(!looks_like_image(b"<html><body>404</body></html>"));
        assert!(!looks_like_image(b""));
    }

    #[test]
    fn album_key_folds_tag_casing() {
        assert_eq!(
            album_key(Some("Owl City"), "Ocean Eyes"),
            album_key(Some("owl city"), "OCEAN EYES")
        );
        // A missing artist is its own key, not merged with a blank-named one.
        assert_eq!(album_key(None, "Ocean Eyes").0, "");
    }

    #[test]
    fn miss_labels_are_distinct_and_carry_the_offending_value() {
        let all = [
            AlbumArtMiss::NoAlbumTag,
            AlbumArtMiss::NoCandidates { query: "q".into() },
            AlbumArtMiss::BelowConfidence {
                best: "b".into(),
                confidence: 0.1,
            },
            AlbumArtMiss::NoCoverArt { mbid: "m".into() },
            AlbumArtMiss::Unavailable { cause: "c".into() },
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|m| m.label()).collect();
        assert_eq!(labels.len(), all.len(), "miss labels must be distinct");

        // A reason that doesn't name the value is another round of guessing —
        // each Display must carry its payload.
        assert!(AlbumArtMiss::NoCandidates {
            query: "releasegroup:\"Ocean Eyes\"".into()
        }
        .to_string()
        .contains("Ocean Eyes"));
        assert!(AlbumArtMiss::NoCoverArt {
            mbid: "abc-123".into()
        }
        .to_string()
        .contains("abc-123"));
        assert!(AlbumArtMiss::Unavailable {
            cause: "connection refused".into()
        }
        .to_string()
        .contains("connection refused"));
        assert!(AlbumArtMiss::BelowConfidence {
            best: "Greatest Hits".into(),
            confidence: 0.42
        }
        .to_string()
        .contains("Greatest Hits"));
    }

    // Every title here MISSED on the first live pass with `no_candidates`.
    // A release GROUP is the abstract album; pressing details belong to an
    // individual release, so a title carrying them matches nothing at all.
    #[test]
    fn edition_qualifiers_are_stripped_from_real_failing_titles() {
        for (raw, want) in [
            (
                "A Rush Of Blood To The Head (Japan)",
                "A Rush Of Blood To The Head",
            ),
            ("Invaders Must Die [Bonus Tracks]", "Invaders Must Die"),
            ("Prospekts March (CD EP UK & Europe)", "Prospekts March"),
            ("Talk (Remixes) (CDr Promo) US", "Talk"),
        ] {
            assert_eq!(strip_edition_qualifiers(raw), want, "stripping {raw:?}");
        }
    }

    #[test]
    fn stripping_never_eats_a_whole_title_or_an_interior_bracket() {
        // Entirely bracketed: stripping would leave nothing to search for.
        assert_eq!(strip_edition_qualifiers("(Untitled)"), "(Untitled)");
        assert_eq!(strip_edition_qualifiers("[]"), "[]");
        // Interior brackets are part of the name, not a suffix.
        assert_eq!(
            strip_edition_qualifiers("Where (Are We)? Now"),
            "Where (Are We)? Now"
        );
        // A plain title is returned unchanged (and costs no extra attempt).
        assert_eq!(strip_edition_qualifiers("Ocean Eyes"), "Ocean Eyes");
        assert_eq!(strip_edition_qualifiers("  Ocean Eyes  "), "Ocean Eyes");
    }

    // MusicBrainz credits a compilation to its contributors, so naming
    // "Various Artists" as the artist EXCLUDES the release being searched for.
    // Every Ministry of Sound compilation missed for exactly this reason.
    #[test]
    fn a_compilation_is_searched_without_its_placeholder_artist() {
        let attempts = search_attempts(
            Some("Various Artists"),
            "Ministry of Sound: Anthems II: 1991-2009",
        );
        assert!(
            attempts.iter().all(|q| !q.contains("Various Artists")),
            "a placeholder artist must never narrow the search: {attempts:?}"
        );
        assert!(attempts[0].contains("Ministry of Sound"));

        for placeholder in ["various artists", "VA", "Soundtrack", "Original Soundtrack"] {
            assert!(
                is_compilation_artist(placeholder),
                "{placeholder} names a compilation, not a performer"
            );
        }
        assert!(!is_compilation_artist("Various Cruelties")); // a real band
        assert!(!is_compilation_artist("Owl City"));
    }

    // The ladder is walked only while an attempt finds NOTHING, so a clean tag
    // that matches costs exactly one rate-limited request. The later rungs are
    // paid for only by albums that would otherwise have stayed blank.
    #[test]
    fn the_exact_query_is_always_tried_first() {
        let attempts = search_attempts(Some("Owl City"), "Ocean Eyes");
        assert_eq!(
            attempts[0], "releasegroup:\"Ocean Eyes\" AND artist:\"Owl City\"",
            "the most specific query must come first"
        );
        // A clean title adds no stripped-title rung — only the drop-the-artist
        // and unquoted last resorts, reached solely when earlier rungs found
        // nothing.
        assert_eq!(attempts.len(), 3, "{attempts:?}");
        assert!(!attempts[1].contains("artist:"));
        assert!(attempts[2].starts_with("releasegroup:("));
    }

    #[test]
    fn a_qualified_title_falls_back_to_the_stripped_one_then_to_no_artist() {
        let attempts = search_attempts(Some("Coldplay"), "A Rush Of Blood To The Head (Japan)");
        assert_eq!(attempts.len(), 4, "{attempts:?}");
        // Most specific first — the exact tag still gets its chance.
        assert!(attempts[0].contains(r"\(Japan\)"));
        assert!(
            attempts[1].contains("A Rush Of Blood To The Head") && !attempts[1].contains("Japan")
        );
        assert!(attempts[1].contains("Coldplay"));
        // Last resort drops the artist: a mis-parsed artist tag still names its
        // album correctly.
        assert!(!attempts[2].contains("artist:"));
        // No duplicates — a repeated query would just burn the rate limit.
        let uniq: std::collections::BTreeSet<_> = attempts.iter().collect();
        assert_eq!(uniq.len(), attempts.len());
    }

    // reqwest's Display stops at "error sending request for url (...)", which
    // names the request and not the failure. Six live misses said exactly that
    // and none said whether it was DNS, TLS, a timeout or a refusal.
    // Every quoted rung is an exact Lucene phrase, so a tag clipped mid-word
    // finds nothing however close it is. `Always Outnumbered Never Outgu` is a
    // real tag in this library.
    #[test]
    fn a_truncated_tag_gets_an_unquoted_rung() {
        let attempts = search_attempts(Some("The Prodigy"), "Always Outnumbered Never Outgu");
        let last = attempts.last().expect("at least one attempt");
        assert_eq!(
            last, "releasegroup:(Always Outnumbered Never Outgu*)",
            "the final rung must be unquoted so Lucene scores partial matches"
        );
        // It is genuinely LAST — the exact phrase still gets first refusal.
        assert!(attempts[0].contains('"'));

        // And the truncated tag would clear the confidence floor once the
        // search returns the real album, which is the whole point of the rung.
        let real = "Always Outnumbered, Never Outgunned";
        let score = pharos_core::title_similarity("Always Outnumbered Never Outgu", real);
        assert!(
            score >= MIN_ALBUM_CONFIDENCE,
            "clipped tag scored {score} against {real}, below the {MIN_ALBUM_CONFIDENCE} floor"
        );
    }

    // B145 — B144's rung was inert for the case it was written for.
    // MusicBrainz ANDs bare terms, so `... Outgu` still required the clipped
    // token to match a real word and returned nothing. The wildcard is the
    // whole point.
    #[test]
    fn the_unquoted_rung_wildcards_its_final_token() {
        assert_eq!(
            unquoted_query("Always Outnumbered Never Outgu").as_deref(),
            Some("releasegroup:(Always Outnumbered Never Outgu*)")
        );
        // A short final token gets NO wildcard — `II*` would match most of the
        // database and hand the similarity floor a pile of noise.
        assert_eq!(
            unquoted_query("Toxicity II").as_deref(),
            Some("releasegroup:(Toxicity II)")
        );
    }

    // `Prospekt's March` split into the terms `Prospekt` and `s`, which ANDed
    // against a stray `s` and matched nothing. Seen live.
    #[test]
    fn an_apostrophe_stays_inside_its_word() {
        assert_eq!(
            unquoted_query("Prospekt's March").as_deref(),
            Some("releasegroup:(Prospekt's March*)")
        );
        // A leading/trailing quote is punctuation, not part of the word.
        assert_eq!(
            unquoted_query("'Heroes'").as_deref(),
            Some("releasegroup:(Heroes*)")
        );
    }

    #[test]
    fn retry_after_is_honoured_and_clamped() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(3)));
        // A malformed or absurd header must not park the backfill for a day.
        h.insert(RETRY_AFTER, HeaderValue::from_static("86400"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(60)));
        h.insert(RETRY_AFTER, HeaderValue::from_static("0"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(1)));
        // The HTTP-date form is not parsed; fall back to the fixed backoff.
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    // A 503 must slow down EVERY caller, not just the one that saw it —
    // otherwise the next queued album walks straight into the same throttle.
    #[tokio::test(start_paused = true)]
    async fn a_throttle_delays_every_waiter_not_just_the_one_that_saw_it() {
        let gate = RateGate::new();
        gate.acquire().await;
        gate.delay(Duration::from_secs(5)).await;
        let before = tokio::time::Instant::now();
        gate.acquire().await;
        assert!(
            tokio::time::Instant::now() - before >= Duration::from_secs(5),
            "the next caller must inherit the backoff"
        );
    }

    #[test]
    fn the_unquoted_rung_drops_lucene_operators_rather_than_escaping_them() {
        // A bare `-` is NOT, a bare `:` opens a field, an unbalanced bracket is
        // a parse error. Unquoted, they have to go entirely.
        assert_eq!(
            unquoted_query("Ministry of Sound: Anthems II: 1991-2009").as_deref(),
            Some("releasegroup:(Ministry of Sound Anthems II 1991 2009*)")
        );
        // Nothing searchable left -> no rung, rather than a query matching
        // everything.
        assert_eq!(unquoted_query("---").as_deref(), None);
        assert_eq!(unquoted_query("").as_deref(), None);
    }

    #[test]
    fn error_chain_reports_the_cause_not_just_the_request() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "dns error: no record found")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "error sending request for url (https://musicbrainz.org/)"
                )
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let rendered = error_chain(&Outer(Inner));
        assert!(rendered.contains("error sending request"));
        assert!(
            rendered.contains("dns error: no record found"),
            "the underlying cause must survive: {rendered}"
        );
    }

    #[test]
    fn front_cover_url_targets_the_release_group_front() {
        assert_eq!(
            front_cover_url("abc-123"),
            "https://coverartarchive.org/release-group/abc-123/front"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_gate_serialises_callers_to_one_per_interval() {
        let gate = Arc::new(RateGate::new());
        let start = tokio::time::Instant::now();
        // Three concurrent callers must leave at least 2 intervals between the
        // first and the last — sleeping in parallel would let all three fire
        // immediately and earn an IP block.
        let mut handles = vec![];
        for _ in 0..3 {
            let g = gate.clone();
            handles.push(tokio::spawn(async move {
                g.acquire().await;
                tokio::time::Instant::now()
            }));
        }
        let mut times = vec![];
        for h in handles {
            times.push(h.await.unwrap());
        }
        times.sort();
        assert!(
            times[2] - start >= MB_MIN_INTERVAL * 2,
            "three calls must span at least two intervals, spanned {:?}",
            times[2] - start
        );
        assert!(times[1] - times[0] >= MB_MIN_INTERVAL);
        assert!(times[2] - times[1] >= MB_MIN_INTERVAL);
    }

    #[test]
    fn album_art_offers_the_cover_as_primary_role() {
        let art = AlbumArt {
            mbid: "abc-123".into(),
            title: "Ocean Eyes".into(),
            confidence: 1.0,
            year: Some(2009),
            bytes: vec![0xFF, 0xD8, 0xFF],
        };
        let remote = art.remote_art();
        assert_eq!(remote.role, ArtworkRole::Primary);
        assert!(remote.url.contains("abc-123"));
    }
}
