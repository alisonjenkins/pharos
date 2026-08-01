//! Resolving a URL into something pharos can transcode — 008.
//!
//! `yt-dlp -j <url>` prints one JSON object describing a video and every format
//! it is available in, WITHOUT downloading anything. That object carries both
//! things pharos needs and they are wanted at different times:
//!
//! - at **ingestion**, the descriptive facts — title, duration, dimensions,
//!   codecs — which become the item's [`MediaProbe`] and never change;
//! - at **playback**, the per-format `url` fields, which are signed, expire, and
//!   have to be fetched again.
//!
//! Only the second is time-sensitive, which is why the stored path is a stable
//! `ytdlp://<extractor>/<id>` and never a URL: the identity survives, the
//! locator does not.
//!
//! # Why resolution and not download
//!
//! The formats yt-dlp reports are ordinary HTTP resources that honour range
//! requests, so ffmpeg can seek into them directly and `-ss` costs one range
//! request rather than a transfer. A design that downloaded first was considered
//! and rejected: for `bestvideo+bestaudio` yt-dlp writes two `.part` files and
//! then runs an ffmpeg MERGE, so the output does not exist until everything has
//! finished — there is no growing file to play from, and any "start when enough
//! has arrived" gate degenerates into waiting for the whole download.
//!
//! [`MediaProbe`]: pharos_core::MediaProbe

pub mod chunks;
pub mod codec;
pub mod source_cache;

use pharos_core::{MediaProbe, RemoteRef};
use serde::Deserialize;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

/// What a resolver failure was, carrying the value that caused it.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("[remote] is not enabled")]
    Disabled,
    #[error("could not run {bin}: {source}")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{bin} exited {status}: {stderr}")]
    Failed {
        bin: String,
        status: String,
        stderr: String,
    },
    #[error("{bin} produced no usable JSON: {source}")]
    Json {
        bin: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{url} resolved to no playable format")]
    NoFormat { url: String },
    #[error("{url} resolved without a usable {field}")]
    MissingField { url: String, field: &'static str },
    #[error("timed out after {secs}s resolving {url}")]
    Timeout { url: String, secs: u64 },
}

impl ResolveError {
    /// Stable metric label. A dashboard keyed on these breaks silently if they
    /// are renamed, so the mapping lives here rather than at each emission.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Spawn { .. } => "spawn",
            Self::Failed { .. } => "exit",
            Self::Json { .. } => "json",
            Self::NoFormat { .. } => "no_format",
            Self::MissingField { .. } => "missing_field",
            Self::Timeout { .. } => "timeout",
        }
    }
}

/// The descriptive half — everything that becomes a library row.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedItem {
    /// The stable identity to store as the item's path.
    pub reference: RemoteRef,
    pub title: String,
    /// Poster/thumbnail to fetch, if the site offered one.
    pub thumbnail: Option<String>,
    pub probe: MediaProbe,
}

/// The time-sensitive half — where the bytes are, right now.
///
/// Two shapes, and which one a site gives you is not a detail: an adaptive
/// source serves video and audio as SEPARATE files, which is the normal case
/// above 720p, while a progressive one serves a single file carrying both.
///
/// Modelled as an enum rather than `{ video, audio: Option<_> }` because that
/// shape cannot distinguish "one file with both streams" from "a video-only
/// file whose audio went missing" — and the two want opposite handling. The
/// first answers an audio request with itself; the second must fail, because
/// using it would produce a silent video and nothing downstream would notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedMedia {
    /// One file carrying both streams.
    Progressive { url: String },
    /// Separate adaptive streams sharing one timeline.
    Adaptive { video: String, audio: String },
}

impl ResolvedMedia {
    /// The locator to hand ffmpeg for a job that wants video, or audio.
    ///
    /// Each HLS rendition is a single input: the CMAF video rung is video-only
    /// (`-an`) and the demuxed audio rung carries no video. A progressive source
    /// answers either question with itself.
    pub fn input_for(&self, wants_video: bool) -> &str {
        match self {
            Self::Progressive { url } => url,
            Self::Adaptive { video, audio } => {
                if wants_video {
                    video
                } else {
                    audio
                }
            }
        }
    }

    /// The video locator, for keying the source generation.
    pub fn video(&self) -> &str {
        match self {
            Self::Progressive { url } => url,
            Self::Adaptive { video, .. } => video,
        }
    }

    /// Every upstream locator this resolution names.
    ///
    /// Derived from the enum rather than hand-listed at the call site, so an
    /// adaptive source cannot have its audio locator silently left behind — the
    /// caller that evicts a superseded resolution has no way to enumerate them
    /// otherwise, and forgetting the audio half would leave a full cached audio
    /// stream on disk answering to a URL nothing will ask for again.
    pub fn urls(&self) -> Vec<&str> {
        match self {
            Self::Progressive { url } => vec![url.as_str()],
            Self::Adaptive { video, audio } => vec![video.as_str(), audio.as_str()],
        }
    }
}

/// yt-dlp's `-j` output, narrowed to the fields pharos reads.
#[derive(Debug, Deserialize)]
struct YtdlpJson {
    id: Option<String>,
    extractor: Option<String>,
    extractor_key: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
    vcodec: Option<String>,
    acodec: Option<String>,
    ext: Option<String>,
    tbr: Option<f64>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    /// Present when yt-dlp had to combine two formats; each entry is one of
    /// them, already narrowed to the chosen selection.
    #[serde(default)]
    requested_formats: Vec<YtdlpFormat>,
    /// The single chosen format's URL, when there was only one.
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct YtdlpFormat {
    url: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
}

impl YtdlpFormat {
    fn has_video(&self) -> bool {
        self.vcodec.as_deref().is_some_and(|c| c != "none")
    }
    fn has_audio(&self) -> bool {
        self.acodec.as_deref().is_some_and(|c| c != "none")
    }
}

/// Runs `yt-dlp` and turns its output into the two halves above.
#[derive(Debug, Clone)]
pub struct RemoteResolver {
    bin: String,
    timeout: Duration,
}

impl RemoteResolver {
    pub fn new(bin: impl Into<String>, timeout: Duration) -> Self {
        Self {
            bin: bin.into(),
            timeout,
        }
    }

    /// Describe a URL without downloading it — the ingestion path.
    pub async fn describe(&self, url: &str) -> Result<ResolvedItem, ResolveError> {
        let json = self.run_json(url).await?;
        let doc: YtdlpJson = self.parse(&json)?;
        item_from_json(url, doc)
    }

    /// Locate the bytes for an already-catalogued item — the playback path.
    ///
    /// Takes the stable reference rather than the original URL because that is
    /// what the row holds; the canonical page URL is reconstructed from it.
    pub async fn locate(&self, r: &RemoteRef) -> Result<ResolvedMedia, ResolveError> {
        let url = canonical_url(r);
        let json = self.run_json(&url).await?;
        let doc: YtdlpJson = self.parse(&json)?;
        media_from_json(&url, doc)
    }

    fn parse(&self, json: &str) -> Result<YtdlpJson, ResolveError> {
        serde_json::from_str(json).map_err(|source| ResolveError::Json {
            bin: self.bin.clone(),
            source,
        })
    }

    /// One `yt-dlp -j` invocation, timed and counted.
    ///
    /// The timeout is not optional politeness: this runs on a request path, and
    /// a site that accepts the connection and then stalls would otherwise hold
    /// the caller — and, at playback, a libav worker — indefinitely.
    async fn run_json(&self, url: &str) -> Result<String, ResolveError> {
        let started = Instant::now();
        let res = self.run_json_inner(url).await;
        let secs = started.elapsed().as_secs_f64();
        metrics::histogram!("pharos_remote_resolve_seconds").record(secs);
        // Labelled by EXTRACTOR so one failing site is distinguishable from a
        // broken resolver — the whole point of having the label. On the error
        // path the extractor is not known yet, so the outcome carries the class
        // instead and the extractor is "unknown"; that asymmetry is deliberate
        // and better than dropping the failures.
        let (extractor, outcome) = match &res {
            Ok(_) => ("unknown", "ok"),
            Err(e) => ("unknown", e.label()),
        };
        metrics::counter!(
            "pharos_remote_resolve_total",
            "extractor" => extractor,
            "outcome" => outcome,
        )
        .increment(1);
        res
    }

    async fn run_json_inner(&self, url: &str) -> Result<String, ResolveError> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-J")
            .arg("--no-warnings")
            .arg("--no-playlist")
            // A resolver must never write to disk: everything downstream assumes
            // the bytes are fetched at play time.
            .arg("--skip-download")
            .arg("--")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd.spawn().map_err(|source| ResolveError::Spawn {
            bin: self.bin.clone(),
            source,
        })?;
        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(source)) => {
                return Err(ResolveError::Spawn {
                    bin: self.bin.clone(),
                    source,
                })
            }
            Err(_) => {
                return Err(ResolveError::Timeout {
                    url: url.to_string(),
                    secs: self.timeout.as_secs(),
                })
            }
        };
        if !out.status.success() {
            // Carry yt-dlp's OWN message: "resolution failed" without the
            // extractor's reason ("video unavailable", "sign in to confirm your
            // age") is another round of guessing. Bounded so a runaway stderr
            // cannot land a megabyte in the log.
            let mut stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            stderr.truncate(512);
            return Err(ResolveError::Failed {
                bin: self.bin.clone(),
                status: out.status.to_string(),
                stderr,
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// A [`RemoteResolver`] plus a short-lived memo of what it last returned.
///
/// Resolution costs a process spawn and a round trip to the site, and a single
/// playback asks for the source once per segment. Without a memo a six-second
/// segment would carry a multi-second resolve, and a site would see one request
/// per segment from one viewer — which is how a source gets rate-limited
/// mid-film.
///
/// The TTL is a ceiling on pharos's own reuse and NOT a claim about the
/// upstream lifetime. A signed URL carries its own expiry, that expiry varies
/// by site and is not always honest, and a URL can be revoked before any TTL
/// elapses — so an expired locator still has to be handled where it is used.
/// What the memo guarantees is only that pharos will not keep one for longer
/// than this.
pub struct ResolverCache {
    resolver: RemoteResolver,
    ttl: Duration,
    entries: tokio::sync::Mutex<std::collections::HashMap<RemoteRef, (Instant, ResolvedMedia)>>,
    /// Where the bytes fetched against a resolution are held, so that a
    /// SUPERSEDED resolution's bytes can be released.
    ///
    /// `None` genuinely means "there is no byte cache" — the listener failed to
    /// start, and a remote item reads straight from upstream — rather than "the
    /// wiring was forgotten". That distinction is why the cache is passed at
    /// construction and not attached afterwards: with a setter, an unwired
    /// resolver and a cacheless one are the same object.
    evict_into: Option<std::sync::Arc<source_cache::SourceCache>>,
}

impl ResolverCache {
    pub fn new(
        resolver: RemoteResolver,
        ttl: Duration,
        evict_into: Option<std::sync::Arc<source_cache::SourceCache>>,
    ) -> Self {
        Self {
            resolver,
            ttl,
            entries: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            evict_into,
        }
    }

    /// Release the bytes held against locators nothing will ask for again.
    ///
    /// A signed URL is the cache's KEY, so a re-resolution does not update an
    /// entry — it strands one and starts another. Over a feature-length watch
    /// the TTL elapses many times, and each lapse strands everything downloaded
    /// under the previous URL. The LRU cap means this is not an unbounded leak,
    /// but it is a priority inversion: bytes for a locator that can never be
    /// requested again sit ahead of bytes being watched, and evicting the dead
    /// entry is the whole reason the cap ever has to evict a live one.
    async fn release(&self, superseded: &ResolvedMedia) {
        let Some(cache) = &self.evict_into else {
            return;
        };
        for url in superseded.urls() {
            cache.forget(url).await;
        }
    }

    /// Where this item's bytes are, resolving only if the memo is cold or stale.
    ///
    /// Holds the lock across the resolve deliberately. Concurrent segments of
    /// one rendition all want the SAME answer, and letting them each spawn a
    /// yt-dlp would multiply exactly the upstream load the memo exists to
    /// avoid — the first caller pays, the rest wait for its answer. The lock is
    /// per-cache rather than per-reference, which is acceptable while a
    /// household plays a handful of remote items at once; if that stops being
    /// true the fix is a per-key lock, not a shorter hold.
    pub async fn locate(&self, r: &RemoteRef) -> Result<ResolvedMedia, ResolveError> {
        let (media, superseded) = {
            let mut entries = self.entries.lock().await;
            if let Some((at, media)) = entries.get(r) {
                if at.elapsed() < self.ttl {
                    return Ok(media.clone());
                }
            }
            let media = self.resolver.locate(r).await?;
            let old = entries.insert(r.clone(), (Instant::now(), media.clone()));
            let superseded = match old {
                // Only when the locator actually MOVED. A site that hands back
                // the same URL after the TTL lapses — a static file, a CDN with
                // no signing — would otherwise have its warm cache thrown away
                // on a timer, turning the memo's expiry into a guaranteed cold
                // read of something that never changed.
                Some((_, prev)) if prev != media => Some(prev),
                _ => None,
            };
            (media, superseded)
        };
        // Outside the memo lock. Eviction touches the cache's own locks and its
        // filesystem, and holding the resolve lock across that would make every
        // concurrent segment of the rendition wait on a disk delete for bytes
        // none of them want.
        if let Some(prev) = superseded {
            self.release(&prev).await;
        }
        Ok(media)
    }

    /// Describe a URL for ingestion. Deliberately NOT memoised: this runs once
    /// when a person adds a link, and the descriptive facts it returns are
    /// persisted, so serving them from a memo could write a stale title for a
    /// video that has since been renamed.
    pub async fn describe(&self, url: &str) -> Result<ResolvedItem, ResolveError> {
        self.resolver.describe(url).await
    }

    /// Forget a memo, so the next request resolves afresh.
    ///
    /// Called when a locator turns out to be dead before its TTL — the case the
    /// TTL cannot cover, because expiry is the site's decision and not ours.
    pub async fn invalidate(&self, r: &RemoteRef) {
        let dropped = self.entries.lock().await.remove(r);
        // Same reasoning as the supersede path in `locate`, and the more
        // clear-cut case: this is called BECAUSE the locator is known dead, so
        // nothing will ever ask for those bytes again.
        if let Some((_, media)) = dropped {
            self.release(&media).await;
        }
    }

    /// Drop every memo and release the bytes held against them.
    ///
    /// For removing a library: a URL-backed item's row goes with the library
    /// root, and without this its cached bytes stay on disk answering to a
    /// locator no item points at. Deliberately whole-cache rather than
    /// per-item — the caller deleting a library holds roots, not `RemoteRef`s,
    /// and re-deriving them would mean parsing paths out of rows that have
    /// already been deleted.
    pub async fn forget_all(&self) {
        let dropped: Vec<ResolvedMedia> = {
            let mut entries = self.entries.lock().await;
            entries.drain().map(|(_, (_, m))| m).collect()
        };
        for media in &dropped {
            self.release(media).await;
        }
    }
}

/// The page URL for a reference, for handing back to yt-dlp.
///
/// Only YouTube is special-cased because only YouTube's canonical form is
/// unambiguous from an id alone. Anything else round-trips through yt-dlp's own
/// `<extractor>:<id>` form, which its extractors accept.
fn canonical_url(r: &RemoteRef) -> String {
    match r.extractor() {
        "youtube" => format!("https://www.youtube.com/watch?v={}", r.id()),
        other => format!("{other}:{}", r.id()),
    }
}

fn item_from_json(url: &str, doc: YtdlpJson) -> Result<ResolvedItem, ResolveError> {
    let id = doc.id.clone().ok_or(ResolveError::MissingField {
        url: url.to_string(),
        field: "id",
    })?;
    let extractor = doc
        .extractor
        .clone()
        .or_else(|| doc.extractor_key.clone().map(|k| k.to_ascii_lowercase()))
        .ok_or(ResolveError::MissingField {
            url: url.to_string(),
            field: "extractor",
        })?;
    let reference = RemoteRef::new(extractor, id).ok_or(ResolveError::MissingField {
        url: url.to_string(),
        field: "extractor/id",
    })?;

    // Duration is not optional here, though yt-dlp treats it as such. Without
    // it the HLS variant playlist has no segment count and renders as 0 s, and
    // the fallback probe at the playlist handler would fire a NETWORK probe on
    // every request to paper over it.
    let duration_ms = doc
        .duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1000.0) as u64)
        .ok_or(ResolveError::MissingField {
            url: url.to_string(),
            field: "duration",
        })?;

    let v = codec::parse_video_codec(doc.vcodec.as_deref().unwrap_or(""));
    let probe = MediaProbe {
        // No local file, so no size. `filesize` is the upstream transfer size,
        // which is a fact about the fetch rather than about a file pharos holds.
        size_bytes: doc.filesize.or(doc.filesize_approx),
        duration_ms: Some(duration_ms),
        container: doc.ext.clone(),
        bitrate_bps: doc
            .tbr
            .filter(|b| b.is_finite() && *b > 0.0)
            .map(|b| (b * 1000.0) as u64),
        video_codec: v.codec,
        video_profile: v.profile,
        video_level: v.level,
        audio_codec: doc.acodec.as_deref().and_then(codec::parse_audio_codec),
        width: doc.width,
        height: doc.height,
        frame_rate_mille: doc
            .fps
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| (f * 1000.0).round() as u32),
        ..Default::default()
    };

    Ok(ResolvedItem {
        reference,
        title: doc
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| url.to_string()),
        thumbnail: doc.thumbnail,
        probe,
    })
}

fn media_from_json(url: &str, doc: YtdlpJson) -> Result<ResolvedMedia, ResolveError> {
    // Adaptive case: yt-dlp already picked one video and one audio format.
    if !doc.requested_formats.is_empty() {
        let video = doc
            .requested_formats
            .iter()
            .find(|f| f.has_video())
            .and_then(|f| f.url.clone());
        let audio = doc
            .requested_formats
            .iter()
            .find(|f| f.has_audio() && !f.has_video())
            .and_then(|f| f.url.clone());
        // BOTH or neither. A video-only adaptive result is not a usable source:
        // playing it would give a picture with no sound, and every layer below
        // here would report success.
        if let (Some(video), Some(audio)) = (video, audio) {
            return Ok(ResolvedMedia::Adaptive { video, audio });
        }
    }
    // Progressive case: one file carrying both.
    doc.url
        .map(|url| ResolvedMedia::Progressive { url })
        .ok_or(ResolveError::NoFormat {
            url: url.to_string(),
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tempfile::TempDir;

    /// A captured `yt-dlp -J` document for an adaptive YouTube video, trimmed
    /// to the fields pharos reads.
    const ADAPTIVE: &str = r#"{
        "id": "dQw4w9WgXcQ",
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "title": "Never Gonna Give You Up",
        "duration": 212.0,
        "thumbnail": "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxres.jpg",
        "width": 1920,
        "height": 1080,
        "fps": 25.0,
        "vcodec": "avc1.640028",
        "acodec": "mp4a.40.2",
        "ext": "mp4",
        "tbr": 2500.5,
        "filesize_approx": 66000000,
        "requested_formats": [
            {"url": "https://cdn.example/video?sig=v", "vcodec": "avc1.640028", "acodec": "none"},
            {"url": "https://cdn.example/audio?sig=a", "vcodec": "none", "acodec": "mp4a.40.2"}
        ]
    }"#;

    /// A REAL `yt-dlp -J` document, captured 2026-08-01 and trimmed to the
    /// fields pharos reads (the signed URLs truncated). The hand-written
    /// fixture above proves the parsing; this one proves the FIELD NAMES, which
    /// is the half that breaks silently when yt-dlp changes its output.
    ///
    /// Note `duration` arrives as an integer here and a float above: both are
    /// valid JSON numbers and both must parse.
    const REAL_CAPTURE: &str = r#"{
        "id": "aqz-KE-bpKQ",
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "title": "Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film",
        "duration": 635,
        "thumbnail": "https://i.ytimg.com/vi/aqz-KE-bpKQ/maxresdefault.jpg",
        "width": 3840,
        "height": 2160,
        "fps": 60,
        "vcodec": "av01.0.13M.08",
        "acodec": "mp4a.40.2",
        "ext": "mp4",
        "tbr": 9369.679999999998,
        "filesize_approx": 743212891,
        "requested_formats": [
                {
                        "url": "https://rr6---sn-8pgbpohxqp5-aigk.googlevideo.com/videoplayb?REDACTED_SIG_0",
                        "vcodec": "av01.0.13M.08",
                        "acodec": "none"
                },
                {
                        "url": "https://rr6---sn-8pgbpohxqp5-aigk.googlevideo.com/videoplayb?REDACTED_SIG_1",
                        "vcodec": "none",
                        "acodec": "mp4a.40.2"
                }
        ]
}"#;

    fn doc(s: &str) -> YtdlpJson {
        serde_json::from_str(s).expect("fixture parses")
    }

    /// The descriptive half becomes a MediaProbe good enough to build a
    /// playlist from, with the codec string taken apart.
    #[test]
    fn a_captured_document_becomes_a_usable_probe() {
        let it = item_from_json("https://youtu.be/dQw4w9WgXcQ", doc(ADAPTIVE)).expect("resolves");
        assert_eq!(it.reference.extractor(), "youtube");
        assert_eq!(it.reference.id(), "dQw4w9WgXcQ");
        assert_eq!(it.title, "Never Gonna Give You Up");
        assert_eq!(it.probe.duration_ms, Some(212_000));
        assert_eq!(it.probe.width, Some(1920));
        assert_eq!(it.probe.frame_rate_mille, Some(25_000));
        assert_eq!(it.probe.bitrate_bps, Some(2_500_500));
        // The codec string was taken apart rather than stored whole.
        assert_eq!(it.probe.video_codec.as_deref(), Some("h264"));
        assert_eq!(it.probe.video_profile.as_deref(), Some("High"));
        assert_eq!(it.probe.video_level, Some(40));
        assert_eq!(it.probe.audio_codec.as_deref(), Some("aac"));
    }

    /// A document with no duration is REFUSED rather than catalogued.
    ///
    /// Accepting it would produce a playable-looking row whose variant playlist
    /// has no segment count and renders as 0 s, and whose every playlist
    /// request fires a network probe to paper over the gap. Failing at
    /// ingestion, where a human is watching, is much cheaper.
    #[test]
    fn a_document_without_a_duration_is_refused_at_ingestion() {
        let no_dur = ADAPTIVE.replace("\"duration\": 212.0,", "");
        let err = item_from_json("u", doc(&no_dur)).expect_err("must refuse");
        assert!(matches!(
            err,
            ResolveError::MissingField {
                field: "duration",
                ..
            }
        ));
        assert_eq!(err.label(), "missing_field");

        // Zero and non-finite are the same case as absent — a 0 s duration is
        // not a duration.
        for bad in ["0", "0.0", "null"] {
            let d = ADAPTIVE.replace("212.0", bad);
            assert!(item_from_json("u", doc(&d)).is_err(), "duration {bad}");
        }
    }

    /// Adaptive sources give video and audio SEPARATELY, and both have to come
    /// back — ffmpeg needs two inputs. Picking the first URL in the list would
    /// silently produce a video with no sound.
    #[test]
    fn an_adaptive_document_yields_both_urls() {
        let m = media_from_json("u", doc(ADAPTIVE)).expect("locates");
        assert_eq!(
            m,
            ResolvedMedia::Adaptive {
                video: "https://cdn.example/video?sig=v".into(),
                audio: "https://cdn.example/audio?sig=a".into(),
            }
        );
        // Each rendition takes ONE of them; the CMAF video rung is video-only
        // and the demuxed audio rung carries no video.
        assert_eq!(m.input_for(true), "https://cdn.example/video?sig=v");
        assert_eq!(m.input_for(false), "https://cdn.example/audio?sig=a");
    }

    /// An adaptive result missing its audio half is REFUSED, not returned as a
    /// video-only source.
    ///
    /// This is the case the old `{ video, audio: Option<_> }` shape could not
    /// express: it looked identical to a progressive file, so the audio rung
    /// would have been handed a video-only stream and the viewer would have got
    /// a picture with no sound, with every layer below reporting success.
    #[test]
    fn an_adaptive_document_missing_its_audio_is_refused() {
        let video_only = r#"{
            "id": "x", "extractor": "youtube", "duration": 10.0,
            "requested_formats": [
                {"url": "https://cdn.example/video", "vcodec": "avc1.640028", "acodec": "none"}
            ]
        }"#;
        let err = media_from_json("https://example/watch", doc(video_only))
            .expect_err("a video-only adaptive result is not a usable source");
        assert_eq!(err.label(), "no_format");
    }

    /// A progressive (single-file) source has no `requested_formats` and its
    /// one URL carries both streams.
    #[test]
    fn a_progressive_document_yields_one_url_and_no_separate_audio() {
        let prog = r#"{
            "id": "x", "extractor": "vimeo", "title": "t", "duration": 10.0,
            "vcodec": "avc1.42c01e", "acodec": "mp4a.40.2",
            "url": "https://cdn.example/both"
        }"#;
        let m = media_from_json("u", doc(prog)).expect("locates");
        assert_eq!(
            m,
            ResolvedMedia::Progressive {
                url: "https://cdn.example/both".into()
            }
        );
        // One file, both streams — so it answers either request with itself.
        assert_eq!(m.input_for(true), "https://cdn.example/both");
        assert_eq!(m.input_for(false), "https://cdn.example/both");
    }

    /// No usable format at all is an error naming the URL, not an empty result
    /// that fails later inside ffmpeg with nothing to point at.
    #[test]
    fn a_document_with_no_format_names_the_url() {
        let empty = r#"{"id": "x", "extractor": "youtube", "duration": 1.0}"#;
        let err = media_from_json("https://example/watch", doc(empty)).expect_err("must fail");
        assert_eq!(err.label(), "no_format");
        assert!(err.to_string().contains("https://example/watch"));
    }

    /// The reference round-trips back to a URL yt-dlp accepts, which is what
    /// makes re-resolution possible from a stored row alone.
    #[test]
    fn a_reference_round_trips_to_a_resolvable_url() {
        let r = RemoteRef::new("youtube", "dQw4w9WgXcQ").expect("valid");
        assert_eq!(
            canonical_url(&r),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        let other = RemoteRef::new("vimeo", "76979871").expect("valid");
        assert_eq!(canonical_url(&other), "vimeo:76979871");
    }

    /// Every field the resolver reads is present, spelled the same way, and the
    /// right type in a real yt-dlp document. This is the assertion that fails
    /// when yt-dlp renames something, instead of production failing.
    #[test]
    fn a_real_captured_document_parses_and_yields_both_halves() {
        let it = item_from_json(
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            doc(REAL_CAPTURE),
        )
        .expect("a real document must resolve");
        assert_eq!(it.reference.extractor(), "youtube");
        assert_eq!(it.reference.id(), "aqz-KE-bpKQ");
        assert!(it.title.contains("Big Buck Bunny"));
        assert_eq!(
            it.probe.duration_ms,
            Some(635_000),
            "integer duration must parse"
        );
        assert_eq!(it.probe.width, Some(3840));
        assert_eq!(it.probe.height, Some(2160));
        assert_eq!(it.probe.frame_rate_mille, Some(60_000));
        assert_eq!(it.probe.audio_codec.as_deref(), Some("aac"));
        // YouTube's "best" is AV1 here, which the parser must recognise as a
        // family even though it carries no profile/level it can take apart.
        assert_eq!(it.probe.video_codec.as_deref(), Some("av1"));
        assert!(it.thumbnail.is_some());

        // And the playback half finds the two adaptive streams.
        let m = media_from_json("u", doc(REAL_CAPTURE)).expect("locates");
        let ResolvedMedia::Adaptive { video, audio } = &m else {
            panic!("a real YouTube document is adaptive, got {m:?}");
        };
        assert!(video.starts_with("https://"));
        assert_ne!(video, audio, "video and audio must be distinct locators");
    }

    /// Every failure label is distinct — they are metric label values, and a
    /// collision would merge two causes into one series.
    #[test]
    fn resolve_error_labels_are_distinct() {
        let all = [
            ResolveError::Disabled,
            ResolveError::Spawn {
                bin: "b".into(),
                source: std::io::Error::other("x"),
            },
            ResolveError::Failed {
                bin: "b".into(),
                status: "1".into(),
                stderr: "e".into(),
            },
            ResolveError::Json {
                bin: "b".into(),
                source: serde_json::from_str::<u8>("[").expect_err("err"),
            },
            ResolveError::NoFormat { url: "u".into() },
            ResolveError::MissingField {
                url: "u".into(),
                field: "id",
            },
            ResolveError::Timeout {
                url: "u".into(),
                secs: 1,
            },
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|e| e.label()).collect();
        assert_eq!(labels.len(), all.len());
        // And the message carries the cause, not just the class: a yt-dlp exit
        // without its stderr is what makes "it didn't work" unactionable.
        assert!(all[2].to_string().contains('e'));
        assert!(all[4].to_string().contains('u'));
    }

    /// A stub standing in for `yt-dlp`: emits `bodies[n]` on its nth call.
    ///
    /// Driving the REAL `ResolverCache::locate` rather than seeding its private
    /// map. The behaviour under test is what happens when a resolution is
    /// DISPLACED, and a test that reaches past `locate` to install the "before"
    /// state would not exercise the displacement at all — it would assert that
    /// a helper it called does what it does.
    fn stub_ytdlp(dir: &std::path::Path, bodies: &[String]) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let counter = dir.join("calls");
        let bin = dir.join("stub-ytdlp");
        let mut script = String::from("#!/bin/sh\n");
        script.push_str(&format!(
            "n=$(cat {c} 2>/dev/null || echo 0)\necho $((n+1)) > {c}\ncase \"$n\" in\n",
            c = counter.display()
        ));
        for (i, body) in bodies.iter().enumerate() {
            // The last arm is the catch-all, so a call beyond the script's end
            // repeats the final answer instead of failing with an empty body —
            // which would look like a resolver error rather than a stable URL.
            let pat = if i + 1 == bodies.len() {
                "*".to_string()
            } else {
                i.to_string()
            };
            script.push_str(&format!(
                "  {pat}) cat <<'YTDLPEOF'\n{body}\nYTDLPEOF\n  ;;\n"
            ));
        }
        script.push_str("esac\n");
        let mut f = std::fs::File::create(&bin).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        f.set_permissions(std::fs::Permissions::from_mode(0o755))
            .unwrap();
        bin
    }

    /// An upstream serving range requests, so the cache holds REAL bytes.
    async fn upstream(body: Vec<u8>) -> (String, actix_web::dev::ServerHandle) {
        use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
        let served = web::Data::new(body);
        let srv = HttpServer::new(move || {
            App::new().app_data(served.clone()).default_service(web::to(
                |req: HttpRequest, body: web::Data<Vec<u8>>| async move {
                    let range = req
                        .headers()
                        .get(actix_web::http::header::RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("bytes="))
                        .and_then(|v| v.split_once('-'))
                        .and_then(|(a, b)| {
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                        });
                    let (a, b) = range.unwrap_or((0, body.len() - 1));
                    let b = b.min(body.len() - 1);
                    HttpResponse::PartialContent()
                        .insert_header((
                            actix_web::http::header::CONTENT_RANGE,
                            format!("bytes {a}-{b}/{}", body.len()),
                        ))
                        .body(body[a..=b].to_vec())
                },
            ))
        })
        .bind(("127.0.0.1", 0))
        .unwrap();
        let base = format!("http://{}", srv.addrs()[0]);
        let srv = srv.run();
        let handle = srv.handle();
        tokio::spawn(srv);
        (base, handle)
    }

    fn adaptive_doc(video: &str, audio: &str) -> String {
        format!(
            r#"{{"id":"dQw4w9WgXcQ","extractor":"youtube","extractor_key":"Youtube",
               "title":"t","duration":212.0,"width":1920,"height":1080,"fps":25.0,
               "vcodec":"avc1.640028","acodec":"mp4a.40.2","ext":"mp4","tbr":2500.0,
               "requested_formats":[
                 {{"url":"{video}","vcodec":"avc1.640028","acodec":"none"}},
                 {{"url":"{audio}","vcodec":"none","acodec":"mp4a.40.2"}}]}}"#
        )
    }

    async fn warm(cache: &source_cache::SourceCache, url: &str) {
        let key = cache.register(url).await.expect("register");
        cache.read(&key, 0, 4096).await.expect("read");
    }

    /// A re-resolution must release the bytes held under the locator it
    /// replaced — BOTH of them, for an adaptive source.
    ///
    /// The cache is keyed by the signed URL, so a re-resolve does not update an
    /// entry, it strands one. Over a feature-length watch the TTL lapses many
    /// times and each lapse strands everything fetched under the previous URL;
    /// the LRU cap then evicts LIVE bytes to make room for the next copy.
    #[tokio::test]
    async fn a_re_resolution_releases_the_bytes_held_under_the_old_locator() {
        let dir = TempDir::new().unwrap();
        let (base, handle) = upstream(vec![7u8; 40_000]).await;

        let first = adaptive_doc(&format!("{base}/v?sig=1"), &format!("{base}/a?sig=1"));
        let second = adaptive_doc(&format!("{base}/v?sig=2"), &format!("{base}/a?sig=2"));
        let bin = stub_ytdlp(dir.path(), &[first, second]);

        let cache = std::sync::Arc::new(
            source_cache::SourceCache::new(
                dir.path().join("cache"),
                crate::bg_io::NetworkGate::new(2),
                u64::MAX,
            )
            .await,
        );
        // TTL zero so the second `locate` re-resolves rather than memo-hits.
        let rc = ResolverCache::new(
            RemoteResolver::new(bin.to_string_lossy().to_string(), Duration::from_secs(10)),
            Duration::ZERO,
            Some(cache.clone()),
        );
        let r = RemoteRef::parse("ytdlp://youtube/dQw4w9WgXcQ").unwrap();

        let one = rc.locate(&r).await.expect("first resolve");
        for u in one.urls() {
            warm(&cache, u).await;
        }
        assert_eq!(cache.len().await, 2, "both adaptive halves are cached");
        let held = cache.held_bytes().await;
        assert!(held > 0, "the cache must actually hold bytes");

        let two = rc.locate(&r).await.expect("re-resolve");
        assert_ne!(two, one, "the stub must have moved the locator");
        assert_eq!(
            cache.len().await,
            0,
            "both halves of the superseded resolution must be released; leaving \
             the audio behind is the shape `ResolvedMedia::urls` exists to stop"
        );
        assert_eq!(cache.held_bytes().await, 0);
        handle.stop(false).await;
    }

    /// ...but a locator that did NOT move keeps its warm cache.
    ///
    /// Without this the memo's TTL would become a guaranteed cold read on a
    /// timer for any source whose URL is stable — a static file, a CDN that
    /// does not sign — throwing away exactly the bytes the cache exists to
    /// keep. The eviction is keyed on the locator CHANGING, not on the memo
    /// expiring.
    #[tokio::test]
    async fn a_re_resolution_that_returns_the_same_locator_keeps_its_warm_cache() {
        let dir = TempDir::new().unwrap();
        let (base, handle) = upstream(vec![3u8; 40_000]).await;

        let same = adaptive_doc(&format!("{base}/v?stable"), &format!("{base}/a?stable"));
        let bin = stub_ytdlp(dir.path(), &[same.clone(), same]);

        let cache = std::sync::Arc::new(
            source_cache::SourceCache::new(
                dir.path().join("cache"),
                crate::bg_io::NetworkGate::new(2),
                u64::MAX,
            )
            .await,
        );
        let rc = ResolverCache::new(
            RemoteResolver::new(bin.to_string_lossy().to_string(), Duration::from_secs(10)),
            Duration::ZERO,
            Some(cache.clone()),
        );
        let r = RemoteRef::parse("ytdlp://youtube/dQw4w9WgXcQ").unwrap();

        let one = rc.locate(&r).await.expect("first resolve");
        for u in one.urls() {
            warm(&cache, u).await;
        }
        let held = cache.held_bytes().await;

        let two = rc.locate(&r).await.expect("re-resolve");
        assert_eq!(one, two, "the stub returns an unchanged locator");
        assert_eq!(
            cache.held_bytes().await,
            held,
            "an unchanged locator must keep its bytes"
        );
        assert_eq!(cache.len().await, 2);
        handle.stop(false).await;
    }

    /// Deleting the library releases everything, because no row points at those
    /// locators any more.
    #[tokio::test]
    async fn forgetting_every_memo_releases_every_cached_source() {
        let dir = TempDir::new().unwrap();
        let (base, handle) = upstream(vec![9u8; 20_000]).await;

        let doc = adaptive_doc(&format!("{base}/v?x"), &format!("{base}/a?x"));
        let bin = stub_ytdlp(dir.path(), &[doc]);

        let cache = std::sync::Arc::new(
            source_cache::SourceCache::new(
                dir.path().join("cache"),
                crate::bg_io::NetworkGate::new(2),
                u64::MAX,
            )
            .await,
        );
        let rc = ResolverCache::new(
            RemoteResolver::new(bin.to_string_lossy().to_string(), Duration::from_secs(10)),
            Duration::from_secs(600),
            Some(cache.clone()),
        );
        let r = RemoteRef::parse("ytdlp://youtube/dQw4w9WgXcQ").unwrap();
        let m = rc.locate(&r).await.expect("resolve");
        for u in m.urls() {
            warm(&cache, u).await;
        }
        assert!(cache.held_bytes().await > 0);

        rc.forget_all().await;
        assert_eq!(cache.len().await, 0, "a deleted library holds no bytes");
        handle.stop(false).await;
    }

    /// A resolver with no cache must not panic or refuse to work — "the
    /// listener failed to start" is a supported state, not a broken one.
    #[tokio::test]
    async fn a_resolver_with_no_cache_still_resolves() {
        let dir = TempDir::new().unwrap();
        let doc = adaptive_doc("https://cdn.example/v", "https://cdn.example/a");
        let bin = stub_ytdlp(dir.path(), &[doc]);
        let rc = ResolverCache::new(
            RemoteResolver::new(bin.to_string_lossy().to_string(), Duration::from_secs(10)),
            Duration::ZERO,
            None,
        );
        let r = RemoteRef::parse("ytdlp://youtube/dQw4w9WgXcQ").unwrap();
        assert!(rc.locate(&r).await.is_ok());
        rc.invalidate(&r).await;
        rc.forget_all().await;
    }
}
