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
//! and one cover download, not twelve of each. What it cannot do is artist
//! photographs: the Cover Art Archive holds release art exclusively, and the
//! providers that do carry artist images all require a key. Album, album-artist
//! and track tiles are covered by the album cover; a bare artist tile is not,
//! and pretending otherwise would just 404 for every render.

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
                    query: release_group_query(artist, album),
                }),
            };
        }

        let query = release_group_query(artist, album);
        let candidates = self.search_release_groups(&query).await?;
        let best = pharos_core::match_best(album, None, &candidates, MIN_ALBUM_CONFIDENCE);

        let outcome = match best {
            Some(m) => m,
            None => {
                // Remember the miss so the album's other tracks don't each
                // spend a rate-limited search rediscovering it.
                self.memo.lock().await.insert(key, None);
                return Err(match candidates.first() {
                    Some(c) => AlbumArtMiss::BelowConfidence {
                        best: c.title.clone(),
                        confidence: pharos_core::match_best(album, None, &candidates, 0.0)
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

    /// One rate-limited release-group search.
    async fn search_release_groups(
        &self,
        query: &str,
    ) -> Result<Vec<SearchCandidate>, AlbumArtMiss> {
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
                cause: format!("release-group search: {e}"),
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AlbumArtMiss::Unavailable {
                cause: format!("release-group search returned {}", status.as_u16()),
            });
        }
        let body = resp.text().await.map_err(|e| AlbumArtMiss::Unavailable {
            cause: format!("reading release-group body: {e}"),
        })?;
        Ok(parse_release_groups(&body))
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
                cause: format!("cover art fetch: {e}"),
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
                cause: format!("reading cover art body: {e}"),
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
