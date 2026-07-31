//! Disk-backed HLS segment cache (T42).
//!
//! HLS players request `.ts` segments serially (and sometimes in
//! parallel during seeks). Without a cache, every request respawns
//! ffmpeg from scratch for the same byte range — wasted CPU + slow
//! seeking on weak hardware.
//!
//! Design:
//! - One file per `(media_id, segment_index)` under
//!   `{root}/{media_id}/{seg}.ts`.
//! - A per-key SHARED RESULT deduplicates concurrent fetches: the first
//!   request registers the segment as in flight and drives the encode on a
//!   detached task, later requests await the value it publishes. Nobody
//!   holds exclusion across the encode, so a slow segment cannot make a
//!   later requester for the same key wait on a lock (B108).
//! - LRU tracking via `(access_counter, key) → bytes`; eviction is
//!   triggered after each insert and runs lazily until total bytes is
//!   under the configured cap.
//! - V6 still holds: a crashed ffmpeg subprocess never poisons the
//!   cache; the writer renames `.tmp → .ts` atomically and removes the
//!   tmp file on failure.

use dashmap::DashMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::Instrument;

use pharos_transcode::scheduler::JobClass;
use pharos_transcode::{
    progress_sidecar_path, FfmpegTranscoder, SegmentAudio, SegmentContainer, SegmentOpts,
    SegmentVideo, TranscodeOptions, VideoCodec,
};
use tokio::io::AsyncReadExt;

#[derive(Debug, thiserror::Error)]
pub enum HlsCacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcode: {0}")]
    Transcode(String),
    #[error("non-utf8 path")]
    NonUtf8Path,
    /// The transcode scheduler declined the job because there was no spare
    /// capacity for it. Distinct from `Transcode` on purpose: for speculative
    /// warm-up this is the scheduler working as intended, and collapsing it
    /// into a generic failure makes deliberate load-shedding read as breakage.
    #[error("transcode scheduler had no spare capacity")]
    SchedulerBusy,
    /// An audio-rendition file did not appear before the read wait's budget ran
    /// out. Carries WHICH budget expired and what the session had produced by
    /// then — a bare `NotFound` here is indistinguishable from "the client asked
    /// for a segment past the end of the media", and the two need opposite
    /// responses.
    #[error(
        "audio segment {name} not ready: {reason} after {waited_ms}ms (session progress: {})",
        match last_progress { Some(n) => format!("a{n}.m4s"), None => "nothing produced".into() }
    )]
    AudioNotReady {
        name: String,
        reason: AudioWaitGiveUp,
        waited_ms: u64,
        last_progress: Option<u32>,
    },
}

/// Which of the three read-wait budgets expired, as a bounded metric label.
///
/// The three mean different things and want different fixes: `NeverStarted` is
/// an ffmpeg that failed to spawn or died before its first segment,
/// `Stalled` is a session that produced and then stopped advancing (finished,
/// wedged, or — the Ghost in the Shell shape — merely slower than the stall
/// budget under load), and `BudgetExhausted` is a session that kept advancing
/// for the whole 30 s and still never reached the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioWaitGiveUp {
    NeverStarted,
    Stalled,
    BudgetExhausted,
}

impl AudioWaitGiveUp {
    /// The `outcome` label. Stable strings — a dashboard keyed on these breaks
    /// silently if renamed, so the mapping lives here and is asserted distinct
    /// in a test rather than written inline at each emission site.
    pub fn label(self) -> &'static str {
        match self {
            Self::NeverStarted => "never_started",
            Self::Stalled => "stalled",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

impl std::fmt::Display for AudioWaitGiveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NeverStarted => "session never produced a segment",
            Self::Stalled => "session stopped advancing",
            Self::BudgetExhausted => "overall wait budget exhausted",
        })
    }
}

#[derive(Debug)]
struct EntryMeta {
    bytes: u64,
    /// Monotonically-increasing access counter; higher = more recent.
    last_used: u64,
}

/// Compound cache key. Audio + subtitle default to a 0 / off sentinel so
/// the cache layout collapses for the common (no client override) case.
/// Video bitrate is rounded to nearest kbps so floating-point negotiation
/// jitter doesn't produce phantom variant files; `0` means "no override"
/// (negotiator-supplied default).
///
/// Named struct, not a tuple (B45-adjacent hardening): the previous
/// 6-tuple `(u64, u32, u32, i32, u32, u32)` was positionally keyed — four
/// same-typed numbers in a row, where one real collision bug already
/// happened (codec-blind keys served an HEVC copy to h264-only clients)
/// and any silent arg-order slip mis-keys the cache. Named fields make
/// that class unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentIdentity {
    media_id: u64,
    seg_index: u32,
    /// 0 = default track (no client override).
    audio_index: u32,
    /// `NO_SUBTITLE` (-1) = no burn-in.
    subtitle_index: i32,
    /// Video bitrate in kbps; 0 = negotiator default, and also the audio-only
    /// rendition segments, which carry no video at all.
    video_bitrate_kbps: u32,
    /// Audio bitrate in kbps; 0 = negotiator default.
    ///
    /// Kept SEPARATE from the video bitrate rather than collapsed into one
    /// "governing" number. Two clients negotiating the same video bitrate and
    /// different audio bitrates produce different bytes, and a single
    /// governing figure took the video bitrate whenever there was one — so
    /// they shared a cache entry and served each other's audio.
    audio_bitrate_kbps: u32,
    /// See `codec_tag` — distinguishes output codec generations.
    codec_tag: u32,
}

const NO_SUBTITLE: i32 = -1;

/// How producing one segment ended.
///
/// Every value here is a failure mode that reached production SILENTLY. A
/// decoder that cannot rebuild its reference list drops frames and ffmpeg
/// still exits 0; a hardware encoder fed an option it rejects emits a broken
/// bitstream and exits 0. Nothing counted either, so the only evidence was a
/// user reporting a frozen picture. These are the counters that make the same
/// class visible without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentOutcome {
    /// Produced and cached.
    Ok,
    /// Carried less video than its duration implies — see `short_of_frames`.
    Short,
    /// Below the minimum plausible size for a segment.
    Empty,
    /// The transcode itself failed.
    Failed,
    /// Deliberately not produced: speculative warm-up declined by the
    /// scheduler because there was no spare capacity. Not a failure — the
    /// system choosing a client request over a guess — but it must still be
    /// countable, or shedding looks like silence.
    Shed,
}

impl SegmentOutcome {
    /// The `outcome` label. Stable strings: a dashboard or alert keyed on
    /// these breaks silently if they are renamed, so the mapping is spelled
    /// out here and asserted in a test rather than written inline at each
    /// emission site.
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Short => "short",
            Self::Empty => "empty",
            Self::Failed => "failed",
            Self::Shed => "shed",
        }
    }
}

/// Which KIND of failure ended a segment, as a bounded metric label.
///
/// `outcome="failed"` alone cannot distinguish "the source file vanished"
/// (a storage incident — the mergerfs outage shape) from "ffmpeg rejected the
/// encode" (a transcode bug). Those need opposite responses, and telling them
/// apart used to mean reading the error string out of a log line that the
/// failure path never wrote.
fn failure_reason(err: &HlsCacheError) -> &'static str {
    match err {
        HlsCacheError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => "source_missing",
        HlsCacheError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => "permission",
        HlsCacheError::Io(_) => "io",
        HlsCacheError::Transcode(_) => "transcode",
        HlsCacheError::NonUtf8Path => "bad_path",
        HlsCacheError::SchedulerBusy => "scheduler_busy",
        HlsCacheError::AudioNotReady { .. } => "audio_not_ready",
    }
}

/// Count one audio-rendition read wait. Emitted on BOTH outcomes: a counter
/// that only fires on failure cannot answer "what fraction of waits 404?",
/// which is the question the Ghost in the Shell stall actually posed.
fn record_audio_wait(outcome: &'static str) {
    metrics::counter!("pharos_audio_wait_total", "outcome" => outcome).increment(1);
}

/// Count one segment production attempt.
fn record_segment_outcome(outcome: SegmentOutcome, class: JobClass) {
    metrics::counter!(
        "pharos_segment_produced_total",
        "outcome" => outcome.label(),
        "reason" => "none",
        "class" => class.label(),
    )
    .increment(1);
}

/// Count one FAILED segment, keeping the reason alongside the outcome.
fn record_segment_failure(outcome: SegmentOutcome, reason: &'static str, class: JobClass) {
    metrics::counter!(
        "pharos_segment_produced_total",
        "outcome" => outcome.label(),
        "reason" => reason,
        "class" => class.label(),
    )
    .increment(1);
}

/// Which of the two hit paths served a cached segment. Bounded label (two
/// values), because the difference is diagnostic: `fast` means the file was
/// already there on the first look, `coalesced` means this request arrived while
/// the SAME key was already being produced and was handed the result.
///
/// A stream that is mostly `coalesced` is one where prefetch and the client keep
/// colliding on the same segment — which reads as a healthy hit rate while every
/// one of those requests paid the full single-flight wait.
///
/// **Renamed from `post_lock` in 006 phase 2a.** There is no lock any more: a
/// coalescing requester awaits a value, it does not queue for exclusion. The old
/// label is a dashboard contract, so this is a deliberate migration, not a
/// silent rename — any panel or alert selecting `hit_path="post_lock"` must be
/// repointed at `coalesced` in the same change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CacheHitPath {
    Fast,
    Coalesced,
}

impl CacheHitPath {
    fn label(self) -> &'static str {
        match self {
            CacheHitPath::Fast => "fast",
            CacheHitPath::Coalesced => "coalesced",
        }
    }
}

/// Record one segment served from cache.
///
/// V91 — symmetry with the `cache miss` line beside it. The miss path recorded
/// twelve fields while the hit path recorded a bare counter increment on ONE of
/// its two branches (the post-lock re-check incremented nothing at all), so
/// "which of these requests were served from cache, and what produced those
/// bytes?" was unanswerable — exactly the question the 2026-07-28 "Disclosure
/// Day" investigation ran aground on, with 647 segment requests and no way to
/// split them into hits and misses.
///
/// Carries the same identity fields as the miss line (codec / burn / audio_idx
/// / seek_secs) so a single query can join the two and reconstruct what a
/// session was actually served, plus `age_secs`, which says whether a hit came
/// from this session's own prefetch or from a cache entry old enough to predate
/// a deploy — the difference between a warm cache and a stale one.
#[allow(clippy::too_many_arguments)]
fn record_cache_hit(
    media_id: u64,
    seg_index: u32,
    bytes: usize,
    opts: &SegmentOpts,
    class: JobClass,
    hit_path: CacheHitPath,
    read_ms: u64,
    age_secs: Option<u64>,
) {
    metrics::counter!(
        "pharos_segment_cache_total",
        "result" => "hit",
        "class" => class.label(),
        "hit_path" => hit_path.label(),
    )
    .increment(1);
    // A cache hit is assumed instant; on a contended or cold PVC it is not,
    // and a slow hit stalls the client exactly like a slow encode while every
    // transcode metric stays clean.
    metrics::histogram!("pharos_segment_cache_read_seconds").record(read_ms as f64 / 1000.0);
    tracing::info!(
        media.id = media_id,
        seg = seg_index,
        bytes,
        read_ms,
        age_secs,
        hit_path = hit_path.label(),
        codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
        burn = opts.burn_subtitle_stream_index.is_some(),
        burn_idx = opts.burn_subtitle_stream_index,
        audio_idx = opts.audio_source_stream_index,
        seek_secs = opts.window.start_seconds(),
        "hls segment served (cache hit)"
    );
}

/// Age of a cached segment file, from its mtime. `None` when the filesystem
/// does not report one — absence is not evidence of a fresh entry, so it stays
/// an Option rather than defaulting to 0.
fn cached_age_secs(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|d| d.as_secs())
}

/// What ffmpeg reported it actually produced, read from the `-progress`
/// sidecar next to `out` — which is removed on the way out, success or not.
/// `(frames, out_time_seconds)`.
///
/// `None` when the sidecar is missing or carries no usable numbers. The
/// completeness check then simply does not fire: a missing report is not
/// evidence of a bad segment, and rejecting on it would fail every segment
/// produced by a path that does not write one.
async fn read_progress(out: &Path) -> Option<(u64, f64)> {
    let sidecar = progress_sidecar_path(out);
    let text = tokio::fs::read_to_string(&sidecar).await.ok();
    let _ = tokio::fs::remove_file(&sidecar).await;
    // ffmpeg appends a whole block of `key=value` lines every reporting
    // interval, so the LAST occurrence of each key is the final state.
    let mut frames: Option<u64> = None;
    let mut out_time_secs: Option<f64> = None;
    for line in text?.lines() {
        match line.split_once('=') {
            Some(("frame", v)) => frames = v.trim().parse().ok().or(frames),
            Some(("out_time_us", v)) => {
                out_time_secs = v
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .map(|us| us / 1e6)
                    .or(out_time_secs);
            }
            _ => {}
        }
    }
    Some((frames?, out_time_secs?))
}

/// `Some(reason)` when the produced segment carries less than it advertises.
///
/// ffmpeg exits 0 after silently dropping frames it could not decode, so the
/// exit status, the byte floor and the cache all read such a segment as a
/// success. This is the check that does not.
///
/// It deliberately tests only what the produced file can answer on its own:
/// that a segment asked for video contains some, and that the encoder reached
/// the duration it was asked for. Catching a segment that is short by a
/// handful of frames would need the source frame rate here, which this layer
/// has no honest way to know; the decode preroll is what prevents those, and
/// `tests/segment_frame_completeness.rs` is what keeps it working.
fn short_of_frames(progress: Option<(u64, f64)>, opts: &TranscodeOptions) -> Option<String> {
    let (frames, out_time_secs) = progress?;
    let encoding_video = opts.video.is_some() && !matches!(opts.video, Some(VideoCodec::Copy));
    if encoding_video && frames == 0 {
        return Some("no video frames at all".to_string());
    }
    if let Some(want) = opts.duration_seconds() {
        if out_time_secs < want * 0.9 {
            return Some(format!(
                "reached only {out_time_secs:.3}s of the requested {want:.3}s"
            ));
        }
    }
    None
}

/// Stable small tag distinguishing the output video codec + CONTAINER so
/// different segment BYTES never share a cache entry. The container matters:
/// the same H264 codec is muxed into mpegts on the `hls1/*.ts` surface but
/// emitted as audio-free fMP4 on the `h264cmaf/*` surface — identical
/// `(media, seg, audio, bitrate)`, totally different bytes. Keying on the codec
/// alone made them COLLIDE: an h264-CMAF request read a previously-cached
/// mpegts segment, fed those bytes to the mp4 parser, and 500'd
/// ("truncated box at offset 0") — a live prod break.
fn codec_tag(
    video: Option<SegmentVideo>,
    audio: Option<SegmentAudio>,
    container: SegmentContainer,
) -> u32 {
    // Bumping a tag orphans every pre-existing cached segment for that codec
    // (LRU reclaims them) — the mechanism used whenever a change alters the
    // BYTES of a segment for a given (media, index) key.
    //
    // Historical tags 1 (Copy), 9 (H265), 10 (Av1) retired with the
    // `SegmentVideo` type (V30). Tag values for the live codecs are preserved
    // so a warm cache survives where the bytes did not change: VP9 fMP4 KEEPS 12.
    match (video, container) {
        // Audio-ONLY rendition segment (music, and the `/hls1/{A64..A256}`
        // audio ladder). There is no video bitrate to key on, so the AUDIO
        // codec + container have to carry the distinction here — otherwise
        // every audio rung of every container collapsed onto tag 0 and the
        // ladder's rungs served each other's bytes.
        (None, _) => match (audio, container) {
            (None, _) => 0,
            (Some(SegmentAudio::Aac), SegmentContainer::Mpegts) => 20,
            (Some(SegmentAudio::Aac), SegmentContainer::Fmp4) => 21,
            (Some(SegmentAudio::Opus), SegmentContainer::Mpegts) => 22,
            (Some(SegmentAudio::Opus), SegmentContainer::Fmp4) => 23,
        },
        // Muxed mpegts H264 (the `hls1/*.ts` surface).
        //
        // 28 (was 25): every muxed segment sliced from a from-0 continuous
        // session on a source whose AUDIO stream starts late carries audio from
        // that offset LATER in the film — measured +1680 ms on a title whose
        // audio starts at 1.700 s. The bytes are cached and would keep playing
        // ahead of picture. Only THIS rung: the fMP4 surfaces are audio-free
        // (`AudioDelivery::Separate`) and slice nothing.
        //
        // 25 (was 24, was 8): the hardware encoders dropped one frame per
        // segment until `-r` pinned them to the source grid, so every segment
        // NVENC produced under tag 24 is 143 frames where the window implies
        // 144. Those bytes are cached and would keep drifting playback by 0.7%
        // forever; bumping orphans them.
        //
        // 24 (was 8): every segment cached while B116 was live is VIDEO-ONLY —
        // the muxed-audio slice was never resolved and the argv fell through to
        // `-an`. Those bytes are cached under the old tag and would be served
        // forever, so the fix alone does not heal a title anyone played during
        // the window: verified by refetching the exact segment after deploying
        // the fix and probing it (`nb_streams: 1`, still). Bumping the tag
        // orphans them and re-transcodes on next request.
        //
        // Only THIS tag moves. The fMP4 rungs (13, 12) are audio-free by
        // design, were never affected, and keep their warm cache.
        (Some(SegmentVideo::H264), SegmentContainer::Mpegts) => 28,
        // Audio-free fMP4 H264 (the demuxed `h264cmaf/*` surface) — a DISTINCT
        // namespace so it never reads muxed mpegts bytes (or vice versa).
        //
        // 26 (was 13): the dropped-frame defect was in the ENCODER, not the
        // container, so every hardware-encoded segment of this rung is short
        // too. Browsers hit this surface.
        (Some(SegmentVideo::H264), SegmentContainer::Fmp4) => 26,
        // VP9 fMP4 segments are AUDIO-FREE (audio is a separate continuous
        // rendition, the A/V-sync fix). VP9 only ever emits fMP4.
        //
        // 27 (was 12, was 7): same encoder-side frame drop. VP9 has no hardware
        // encoder configured today, so this is precautionary rather than
        // known-bad — but a tag that MIGHT name short segments is not one to
        // keep serving.
        (Some(SegmentVideo::Vp9), _) => 27,
    }
}

/// The bitrate that actually determines a segment's bytes: the video bitrate
/// when there is video, else the audio bitrate.
///
/// An audio-only rendition segment carries `video_bitrate_bps: None`, so keying
/// on the video bitrate alone gave EVERY rung of the audio ladder
/// (`/hls1/{A64,A96,A128,A192,A256}/{seg}.ts`, advertised as separate
/// `EXT-X-STREAM-INF`s for music items) the identical key — whichever rung
/// transcoded first was then served for all of them, silently defeating audio
/// ABR and handing a 64 kbps client the 256 kbps bytes (or the reverse).
impl SegmentIdentity {
    /// The one derivation of a segment's identity. The on-disk cache path and
    /// the HTTP ETag both come from this value, so they cannot describe
    /// different things — a hand-rolled ETag that restated a hand-picked
    /// subset of these inputs had already drifted from the cache key once,
    /// and served one variant's bytes under another's 304.
    pub fn new(
        media_id: u64,
        seg_index: u32,
        audio_index: Option<u32>,
        subtitle_index: Option<u32>,
        opts: &SegmentOpts,
    ) -> Self {
        let kbps = |b: Option<u64>| {
            b.map(|b| (b / 1000).min(u32::MAX as u64) as u32)
                .unwrap_or(0)
        };
        Self {
            media_id,
            seg_index,
            // Only a segment that CARRIES audio is keyed by the audio track.
            // The fMP4 surfaces deliver audio as a separate rendition and end
            // their argv in `-an`, so their bytes are identical across tracks;
            // keying them apart minted a second, byte-identical video ladder
            // the moment a viewer switched audio track.
            audio_index: match opts.audio_codec() {
                Some(_) => audio_index.unwrap_or(0),
                None => 0,
            },
            subtitle_index: subtitle_index.map(|n| n as i32).unwrap_or(NO_SUBTITLE),
            video_bitrate_kbps: kbps(opts.video_bitrate_bps),
            audio_bitrate_kbps: kbps(opts.audio_bitrate_bps()),
            codec_tag: codec_tag(opts.video, opts.audio_codec(), opts.container),
        }
    }

    /// Cache filename. `{seg}-a{A}-s{S}-v{V}-a{Abr}-c{tag}.ts`.
    fn filename(&self) -> String {
        // Destructured WITHOUT `..` deliberately: a new identity field then
        // fails to compile here until someone decides whether it belongs in
        // the name. A field that silently stays out of the filename is a
        // cache collision, which is how the audio ladder came to serve one
        // rung's bytes for all five.
        let Self {
            // The media id is the containing directory, not part of the name.
            media_id: _,
            seg_index,
            audio_index,
            subtitle_index,
            video_bitrate_kbps,
            audio_bitrate_kbps,
            codec_tag,
        } = self;
        let sub = if *subtitle_index == NO_SUBTITLE {
            "off".to_string()
        } else {
            subtitle_index.to_string()
        };
        let br = |k: u32| {
            if k == 0 {
                "auto".to_string()
            } else {
                k.to_string()
            }
        };
        format!(
            "{seg_index}-a{audio_index}-s{sub}-v{}-b{}-c{codec_tag}.ts",
            br(*video_bitrate_kbps),
            br(*audio_bitrate_kbps),
        )
    }

    /// The segment's location under the cache root: `{media}/{filename}`.
    /// This is the identity in full — the media id lives in the directory
    /// rather than the file name.
    fn cache_relative_path(&self) -> String {
        format!("{}/{}", self.media_id, self.filename())
    }

    /// HTTP ETag for these bytes. Hashes the SAME string that locates the
    /// cache entry, so a segment whose bytes would change necessarily gets a
    /// new ETag, and two requests that resolve to one cache entry always
    /// present the same one.
    pub fn etag(&self) -> String {
        use xxhash_rust::xxh3::xxh3_64;
        let h = xxh3_64(self.cache_relative_path().as_bytes()) & 0x7FFF_FFFF_FFFF_FFFF;
        format!("W/\"seg-{h:016x}\"")
    }
}

#[derive(Debug, Default)]
struct CacheState {
    /// Per-directory locks deduplicating continuous-audio HLS sessions (the
    /// A/V-sync fix): the first request spawns the one ffmpeg producing the
    /// audio rendition; concurrent requests see it already running.
    audio_locks: HashMap<PathBuf, Arc<Mutex<()>>>,
    entries: HashMap<SegmentIdentity, EntryMeta>,
    total_bytes: u64,
    access_counter: u64,
    /// TRUE first-sample time of each continuous-audio session file, probed
    /// once and reused. See `probed_session_start`.
    audio_session_starts: HashMap<PathBuf, f64>,
    /// Where each source's audio track begins, in seconds, keyed by
    /// `(source, audio-relative index)`. A property of the file, so probed once
    /// and reused. See `source_audio_start`.
    source_audio_starts: HashMap<(PathBuf, u32), f64>,
}

/// One file of the demuxed audio rendition, with the session that produced it.
///
/// The session start is not bookkeeping: ffmpeg's HLS muxer numbers `tfdt` from
/// a session's OWN first fragment, so `a300.m4s` written by a session that
/// started at segment 225 carries 450 s — its position WITHIN the session — not
/// the 1800 s the playlist places it at. Only the caller that knows the start
/// can put the fragment back on the timeline (B121).
pub struct AudioRenditionFile {
    pub bytes: Vec<u8>,
    pub session_start_seg: u32,
}

/// Outcome of [`HlsSegmentCache::choose_audio_start_seg`]: reuse a session
/// already covering the request, or spawn one starting at the given segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioStart {
    Reuse,
    Start(u32),
}

#[derive(Clone)]
pub struct HlsSegmentCache {
    root: PathBuf,
    max_bytes: u64,
    transcoder: FfmpegTranscoder,
    /// When set, segment transcodes are dispatched through the
    /// load-balancing scheduler (multi-GPU + all-CPU, crash-isolated
    /// workers writing the segment file directly) instead of the inline
    /// `transcoder`. `None` keeps the legacy single-ffmpeg path (tests,
    /// or builds without a worker binary).
    scheduler: Option<pharos_transcode::scheduler::TranscodeScheduler>,
    state: Arc<Mutex<CacheState>>,
    /// Segments currently being produced, keyed by identity. The value carries
    /// a `watch` receiver holding the eventual outcome.
    ///
    /// This is single-flight WITHOUT mutual exclusion: nobody holds anything
    /// across the encode, so a slow segment cannot make a later requester for
    /// the same key wait on a lock — it waits on the answer, and gets it the
    /// instant the encode does. The predecessor, a per-key `Mutex` held for the
    /// whole multi-second transcode, is what made a queued speculative job
    /// poisonous: the client's own later request for that segment inherited the
    /// entire wait (B108).
    inflight: Arc<DashMap<SegmentIdentity, InFlightSegment>>,
}

/// The eventual outcome of one in-flight segment encode, as every waiter sees
/// it.
///
/// Both sides are behind an `Arc` because the value is shared: the bytes so one
/// encode is not copied per waiter until it is returned, and the error because
/// [`HlsCacheError`] is not `Clone` (`std::io::Error` is not). It is deliberately
/// NOT a `String`: collapsing the error would lose the VARIANT, and
/// `SchedulerBusy` — deliberate load-shedding — would come back to callers as a
/// generic transcode failure. See [`shared_copy`].
type SharedSegment = Result<Arc<Vec<u8>>, Arc<HlsCacheError>>;

/// A segment somebody is already producing.
#[derive(Clone)]
struct InFlightSegment {
    /// Resolves to `Some(outcome)` exactly once, when the driver publishes.
    rx: tokio::sync::watch::Receiver<Option<SharedSegment>>,
}

/// Rebuild an owned error for a waiter out of the one the driver produced.
///
/// The variant is preserved because callers act on it (`SchedulerBusy` is
/// shedding, not breakage) and `failure_reason` labels metrics from it. The
/// `Io` arm keeps the kind and the full message; only the OS error's raw code
/// object is left behind, which nothing here reads.
fn shared_copy(err: &HlsCacheError) -> HlsCacheError {
    match err {
        HlsCacheError::Io(e) => HlsCacheError::Io(std::io::Error::new(e.kind(), e.to_string())),
        HlsCacheError::Transcode(m) => HlsCacheError::Transcode(m.clone()),
        HlsCacheError::NonUtf8Path => HlsCacheError::NonUtf8Path,
        HlsCacheError::SchedulerBusy => HlsCacheError::SchedulerBusy,
        HlsCacheError::AudioNotReady {
            name,
            reason,
            waited_ms,
            last_progress,
        } => HlsCacheError::AudioNotReady {
            name: name.clone(),
            reason: *reason,
            waited_ms: *waited_ms,
            last_progress: *last_progress,
        },
    }
}

/// Drops the in-flight registration however the driver ends.
///
/// A `Drop` impl rather than a line at the end of the driver because the entry
/// must go on EVERY exit — including a panic. An entry left behind after its
/// sender is gone would make that key permanently unrequestable: every later
/// request would find the registration, await a channel nobody can publish to,
/// and get the dead-driver error forever.
struct InFlightGuard {
    map: Arc<DashMap<SegmentIdentity, InFlightSegment>>,
    key: SegmentIdentity,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.map.remove(&self.key);
    }
}

impl std::fmt::Debug for HlsSegmentCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlsSegmentCache")
            .field("root", &self.root)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

/// B41 — bump whenever segment GENERATION changes in a way that makes
/// previously-cached segments incompatible with fresh ones (e.g. the
/// mpegts `-output_ts_offset` fix: old segments carry PTS≈0, new ones carry
/// their true timeline position — mixing them in one hls.js session breaks
/// buffering). A mismatch with the on-disk `.gen_version` wipes the cache at
/// construction; segments regenerate on demand (cheap: only what's watched).
///
/// v3 (B45): stream-copied mpegts segments abolished (PTS reset per segment,
/// keyframe-sloppy durations, 6ch AAC) and re-encoded segments gained
/// `-muxdelay 0` (old ones carry a +1.4 s skew) — every cached `.ts` from
/// v2 is poisoned.
///
/// v4 (B105): the VP9 continuous-audio rendition now frame-snaps its seek
/// anchor to the video grid instead of the nominal `seg*6.0`. Stale
/// `_audiohls` dirs carry nominal-anchored segments that desync against the
/// video — orphan them so a fresh, aligned session regenerates on demand.
///
/// v5: segment boundaries are computed in exact frames, and a source whose
/// `frame_rate_mille` was really the 90 kHz container clock no longer snaps to
/// a bogus grid (see `pharos_core::FrameRate`). Segments cached under the old
/// grid start a sub-frame away from where the new playlist says they do — the
/// encoder duplicated or dropped the boundary frame when producing them, which
/// is the stutter this fixes — so they must not be reused.
///
/// v6: segments are produced with a decode preroll. Every segment cached
/// under v5 whose decoder could not rebuild its reference picture set at the
/// seek point is missing frames — up to all of them — and ffmpeg exited 0 for
/// each, so nothing marked them bad. They cannot be told apart from good ones
/// on disk, and a re-request would serve the frozen picture forever. Orphan
/// the generation.
///
/// v7: segments mux their audio by COPY from the title's one continuous
/// encode instead of encoding it per segment. Every v6 segment carries audio
/// on its own frame grid, phase-shifted against its neighbours' — the drift
/// this fixes — so they must not be reused.
///
/// v8: v7 segments took their muxed audio from the WRONG position. `-ss` on
/// an input is relative to that input's own start time, so a segment seeking
/// the continuous encode by an absolute source position landed at
/// `session_start + position` — past the end for any seek session, so the
/// audio was silence or an unrelated stretch of the title under correct
/// video. Every v7 segment served from a seek session is poisoned.
///
/// v9: the cache filename carries the video AND audio bitrates separately
/// (`-v{V}-b{A}`) instead of one "governing" figure that took the video
/// bitrate whenever there was one. v8 names cannot be parsed under the new
/// scheme and, worse, v8 entries conflated two clients whose audio bitrates
/// differed.
/// v11: every fMP4 segment now carries a pinned track timescale
/// ([`pharos_transcode::FMP4_TRACK_TIMESCALE`]) instead of inheriting one from
/// whichever device encoded it. Cached v10 segments are a mix of 90000
/// (software) and 24000 (hardware) entries, and the mismatched half is read on
/// the init's clock and lands nowhere near its true position — so they cannot
/// be reused alongside the pinned ones.
/// v12: h264 CMAF (fMP4) segments are now pinned to libx264 (CPU) so a whole
/// shared-init rendition comes from ONE encoder. v11 CMAF caches hold a mix of
/// NVENC-init + libx264-segment (and vice versa) entries whose SPS is
/// incompatible with the init — undecodable in the browser (issue #114) — so
/// every v11 h264 CMAF entry must be orphaned.
// 13 (spec 003): CMAF H264 renditions moved from "always CPU" to a
// deterministic device that resolves to hardware. Every segment cached under
// the old rule was produced by libx264, and the init a client already holds may
// have been too -- so a freshly encoded NVENC segment would decode under a
// libx264 init and fail (issue #114). The bytes are not wrong, their PAIRING
// is, and nothing in the cache path can tell them apart.
//
// R8 argued this bump was unnecessary because a deterministic device means
// cached bytes always came from the device the rendition still resolves to.
// That holds only while the RULE is fixed; the deploy that introduced the rule
// is itself the moment it changes. Observed live: 1489 cache hits (CPU-era)
// against 11 fresh NVENC prefetch encodes for one episode.
// 15: the CMAF hardware change is RE-LANDED, flipping the assignment rule from
// CPU-only back to a deterministic per-rendition device. Third move of this
// rule, third bump: by V89 every artefact cached under the previous assignment
// is stale the moment the rule changes, in EITHER direction — the 14 bump was
// the revert, this one is the re-land. Without it the ~5000 libx264 CMAF
// segments cached under 14 would be served beneath an NVENC-produced init,
// which is issue #114 reached from disk instead of from the load balancer.
/// Public because the HLS playlists embed it in every init/segment URI: a
/// browser caches those `immutable` for a year, so a generation change must
/// change the URL or clients keep serving themselves the previous
/// generation's init (see `hls::rendition_qs`). That property is what makes
/// this re-land safe where the first attempt was not: the bump is now visible
/// to the CLIENT cache, not only to the server's.
pub const HLS_GEN_VERSION: u32 = 15;
const GEN_VERSION_MARKER: &str = ".gen_version";

impl HlsSegmentCache {
    pub fn new(root: impl Into<PathBuf>, max_bytes: u64) -> Self {
        let root: PathBuf = root.into();
        Self::reconcile_generation(&root);
        Self {
            root,
            max_bytes,
            transcoder: FfmpegTranscoder::new(),
            scheduler: None,
            state: Arc::new(Mutex::new(CacheState::default())),
            inflight: Arc::new(DashMap::new()),
        }
    }

    /// Wipe every cached segment when the on-disk generation version doesn't
    /// match [`HLS_GEN_VERSION`] (same pattern as the trickplay cache).
    /// Best-effort: fs errors leave the cache as-is rather than failing boot.
    fn reconcile_generation(root: &std::path::Path) {
        let marker = root.join(GEN_VERSION_MARKER);
        let on_disk = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if on_disk == Some(HLS_GEN_VERSION) {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten() {
                let p = e.path();
                if p.file_name().and_then(|n| n.to_str()) == Some(GEN_VERSION_MARKER) {
                    continue;
                }
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(&p);
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        let _ = std::fs::create_dir_all(root);
        let _ = std::fs::write(&marker, HLS_GEN_VERSION.to_string());
    }

    /// Route segment transcodes through the load-balancing scheduler.
    /// Each segment is dispatched to the least-loaded eligible device
    /// (every GPU + the CPU), encoded by a crash-isolated worker that
    /// writes the `.ts` file directly (no cross-process byte copy).
    pub fn with_scheduler(
        mut self,
        sched: pharos_transcode::scheduler::TranscodeScheduler,
    ) -> Self {
        self.scheduler = Some(sched);
        self
    }

    /// Override the ffmpeg binary path. Used by the integration tests
    /// to point at a nix-store-pinned binary; production reads from
    /// `$PATH`.
    pub fn with_ffmpeg(mut self, p: impl Into<PathBuf>) -> Self {
        self.transcoder = FfmpegTranscoder::with_binary(p);
        self
    }

    /// P14 — attach a hardware encoder to the underlying transcoder.
    /// Pass `HwAccel::Off` for the software path.
    pub fn with_hwaccel(mut self, accel: pharos_transcode::HwAccel) -> Self {
        self.transcoder = self.transcoder.clone().with_hwaccel(accel);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Fetch the bytes for `(media_id, seg_index)` with no per-track
    /// override. Equivalent to `segment_bytes_keyed(.., None, None,
    /// ..)`. Retained for callers that don't know about per-stream
    /// indices yet.
    pub async fn segment_bytes(
        &self,
        media_id: u64,
        seg_index: u32,
        source: &Path,
        opts: &SegmentOpts,
        class: JobClass,
    ) -> Result<Vec<u8>, HlsCacheError> {
        self.segment_bytes_keyed(media_id, seg_index, None, None, source, opts, class)
            .await
    }

    /// W1/W2 — per-stream cache lookup. `audio_index` + `subtitle_index`
    /// land in the cache key + the on-disk path so a client switching
    /// audio track doesn't trample the previous track's cached
    /// segments. None values fall through to the default-track sentinel
    /// (audio=0, subtitle=-1).
    /// V30 — this is the ONLY segment-mint entry point, and it accepts only
    /// [`SegmentOpts`]: a stream-copied or progressive-container segment is
    /// a compile error, not a code-review catch.
    ///
    /// `class` says who is waiting on the mint: [`JobClass::Interactive`] when
    /// a client HTTP response is blocked on these bytes, [`JobClass::Background`]
    /// when this is speculative warm-up. Both classes previously reached the
    /// transcode scheduler as identical jobs, which is why a segment a browser
    /// was waiting for could sit behind a pile of segments nobody had asked for
    /// with nothing in any log or metric to say so.
    // The parameter list is the cache key plus the caller's intent, and V30
    // makes this the single mint entry point on purpose — collapsing the
    // dimensions into a struct would hide which of them key the cache.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        name = "segment_cache",
        skip_all,
        fields(media.id = media_id, seg = seg_index, class = class.label())
    )]
    pub async fn segment_bytes_keyed(
        &self,
        media_id: u64,
        seg_index: u32,
        audio_index: Option<u32>,
        subtitle_index: Option<u32>,
        source: &Path,
        opts: &SegmentOpts,
        class: JobClass,
    ) -> Result<Vec<u8>, HlsCacheError> {
        let key = SegmentIdentity::new(media_id, seg_index, audio_index, subtitle_index, opts);
        let path = self.segment_path_keyed(key);

        // Fast hit path: file present, just bump LRU. A concurrent
        // eviction can delete the file between the stat and the read; treat
        // that NotFound as a miss and fall through to regenerate rather
        // than surfacing a spurious 500 on a genuine cache hit.
        //
        // `metadata` rather than `try_exists`: the same syscall answers "is it
        // there?" and carries the mtime the hit line reports as `age_secs`, so
        // the added observability costs no extra stat.
        let hit_started = std::time::Instant::now();
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            self.touch(key).await;
            match tokio::fs::read(&path).await {
                Ok(b) => {
                    record_cache_hit(
                        media_id,
                        seg_index,
                        b.len(),
                        opts,
                        class,
                        CacheHitPath::Fast,
                        hit_started.elapsed().as_millis() as u64,
                        cached_age_secs(&meta),
                    );
                    return Ok(b);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* evicted; fall through */
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Coalesce onto an encode already in progress for this exact key, or
        // become the one that starts it, then wait for the ANSWER rather than
        // for exclusion.
        //
        // At most TWO registration attempts. The second exists only for the
        // inherited-shed fall-through below, and it is explicitly barred from
        // falling through again (`attempt == 1`), so no interleaving — however
        // pathological — can spin here: every path out of the loop is either a
        // published outcome or the second attempt's outcome, whatever it is.
        let coalesced_started = std::time::Instant::now();
        let mut attempt = 0u8;
        let (outcome, driving) = loop {
            attempt += 1;
            let (mut rx, driving) =
                self.register_or_join(key, source, opts, media_id, seg_index, class);
            let outcome = Self::await_segment(&mut rx).await;
            // A request that did not drive this encode must not be failed by
            // its driver's LOAD-SHED decision. `SchedulerBusy` says "the
            // scheduler kept its permits for work a client is blocked on" — the
            // right answer for the speculative prefetch that asked, and the
            // wrong one for a client waiting on the segment, which reaches the
            // browser as a 500 on a video segment. Prefetch, prewarm and burn
            // warm-up all submit as `Background`, and Background is shed
            // precisely when load is high, so this is load-correlated: shed
            // probability approaches 1 exactly when an interactive request most
            // needs to be admitted. Under the per-key mutex this replaced, the
            // waiter re-checked the filesystem after the shed and submitted its
            // OWN job, at its OWN class.
            //
            // So: fall through and drive it. This is NOT promotion — nobody's
            // class is changed and the shed job is not resubmitted; the
            // requester simply declines to adopt a decision made about somebody
            // else's job. A DRIVING requester keeps its own shed (B108/V58):
            // shedding work you submitted yourself is the intended behaviour.
            let inherited_shed = !driving
                && matches!(&outcome, Err(e) if matches!(**e, HlsCacheError::SchedulerBusy));
            if !(inherited_shed && attempt == 1) {
                break (outcome, driving);
            }
            tracing::debug!(
                media.id = media_id,
                seg = seg_index,
                class = class.label(),
                "hls segment: re-driving rather than inheriting another job's shed"
            );
            // The driver publishes and only THEN drops its guard, so between
            // those two points the registration is a corpse: an entry whose
            // outcome is already decided. Re-registering onto it would hand
            // back the same shed and waste the one re-attempt. The window is a
            // few instructions wide and only reachable across threads, so one
            // `yield_now` is enough to let the guard run; if it somehow is not,
            // the second attempt inherits the shed and we are exactly where
            // this branch found us, never worse. Matched by channel identity so
            // a NEW driver's registration is never mistaken for the corpse.
            let corpse_still_registered = self
                .inflight
                .get(&key)
                .is_some_and(|e| e.rx.same_channel(&rx));
            if corpse_still_registered {
                tokio::task::yield_now().await;
            }
        };

        let bytes = outcome.map_err(|e| shared_copy(&e))?;
        if !driving {
            // Served by somebody else's encode. This is a hit — it costs a wait
            // but no work — and it is the successor to the old `post_lock` path.
            // Same fields as the fast hit; `age_secs` is None because these
            // bytes never came off a file whose mtime could be asked.
            self.touch(key).await;
            record_cache_hit(
                media_id,
                seg_index,
                bytes.len(),
                opts,
                class,
                CacheHitPath::Coalesced,
                coalesced_started.elapsed().as_millis() as u64,
                None,
            );
        }
        Ok(bytes.as_ref().clone())
    }

    /// Register as the driver of this key's encode, or join the one already in
    /// progress. Returns the receiver the outcome will be published on, and
    /// whether THIS call is the one driving it.
    ///
    /// The registration is made under the DashMap's shard lock, which is
    /// released before the driver is spawned, and the driver is DETACHED — so
    /// nothing is held across the encode and nothing about it depends on the
    /// requester surviving.
    #[allow(clippy::too_many_arguments)]
    fn register_or_join(
        &self,
        key: SegmentIdentity,
        source: &Path,
        opts: &SegmentOpts,
        media_id: u64,
        seg_index: u32,
        class: JobClass,
    ) -> (
        tokio::sync::watch::Receiver<Option<SharedSegment>>,
        bool, // driving
    ) {
        let (rx, tx) = match self.inflight.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(e) => (e.get().rx.clone(), None),
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                e.insert(InFlightSegment { rx: rx.clone() });
                (rx, Some(tx))
            }
        };
        let Some(tx) = tx else {
            return (rx, false);
        };
        let driver = self.clone();
        let driver_source = source.to_path_buf();
        let driver_opts = opts.clone();
        // Built HERE, outside the spawned block, and MOVED into it. Built
        // inside, a task dropped before its FIRST poll — which is what runtime
        // shutdown does to a freshly spawned task — would never construct the
        // guard while its captured `tx` dropped anyway, leaving a registration
        // whose channel is closed. Every later request for that key would then
        // find the registration, await a channel nobody can publish to, and get
        // the dead-driver error forever: precisely the state the guard exists to
        // prevent. Owned by the future, it drops with the future whether or not
        // the future is ever polled.
        let guard = InFlightGuard {
            map: self.inflight.clone(),
            key,
        };
        // Carry this request's span into the detached task. Without it the
        // encode's own lines and its `write_segment` span land unparented, and
        // the miss line loses the request it belongs to — the trace would end
        // where the work begins.
        let span = tracing::Span::current();
        tokio::spawn(
            async move {
                // Named so the block CAPTURES it: registered until the driver
                // ends, panic included.
                let _guard = guard;
                let out = driver
                    .produce_segment(
                        &driver_source,
                        &driver_opts,
                        key,
                        media_id,
                        seg_index,
                        class,
                    )
                    .await
                    .map(Arc::new)
                    .map_err(Arc::new);
                // Publish BEFORE the guard removes the entry, so a requester
                // that cloned the receiver a moment ago always sees a value.
                let _ = tx.send(Some(out));
            }
            .instrument(span),
        );
        (rx, true)
    }

    /// Wait for the driver to publish this segment's outcome.
    ///
    /// Two iterations at most: the value is either already published or arrives
    /// with the next change, and the only value ever sent is `Some(_)`.
    async fn await_segment(
        rx: &mut tokio::sync::watch::Receiver<Option<SharedSegment>>,
    ) -> SharedSegment {
        loop {
            let published = rx.borrow_and_update().clone();
            if let Some(v) = published {
                return v;
            }
            if rx.changed().await.is_err() {
                // Every sender is gone and nothing was published: the driver
                // panicked, or the runtime is shutting down. Say so and let the
                // caller retry — the registration is already gone with it, so
                // the next request re-drives — rather than waiting forever on a
                // channel nobody can send to.
                return Err(Arc::new(HlsCacheError::Transcode(
                    "segment encode driver stopped without publishing a result \
                     (the task panicked or the runtime is shutting down)"
                        .to_string(),
                )));
            }
        }
    }

    /// Produce one segment: transcode to a temp file, validate, publish into
    /// the keyed cache path, record.
    ///
    /// Runs in a DETACHED task, so it outlives the requester that started it —
    /// a client that seeks away mid-encode no longer throws away work that the
    /// next requester will immediately ask for again. Under the per-key `Mutex`
    /// it replaced, that cancellation dropped the guard, and the next waiter
    /// re-checked the filesystem, found nothing, and encoded the same segment a
    /// second time.
    async fn produce_segment(
        &self,
        source: &Path,
        opts: &SegmentOpts,
        key: SegmentIdentity,
        media_id: u64,
        seg_index: u32,
        class: JobClass,
    ) -> Result<Vec<u8>, HlsCacheError> {
        let path = self.segment_path_keyed(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("ts.tmp");
        // Time the transcode: a segment covers SEGMENT_SECONDS of playback, so
        // if this exceeds that wall-clock the encoder is below realtime and the
        // client will stall. Logged per miss so Loki/Tempo show exactly which
        // segments are slow and why (codec + subtitle burn are the usual cost).
        let started = std::time::Instant::now();
        // Resolve the audio HERE rather than in each delivery handler, so no
        // path can mint a segment that encodes its own audio. That is exactly
        // how the muxed mpegts surface ended up with a per-segment AAC encode
        // while the browser surface had a continuous one.
        //
        // `resolve` runs the closure for exactly the deliveries that need a
        // slice, and moves the result into the value being built — so lowering
        // a muxed segment without one is not something this code can express.
        let start_secs = opts.window.start_seconds();
        let dur_secs = opts.window.duration_seconds();
        let resolved = opts
            .clone()
            .resolve(|c| {
                self.ensure_continuous_audio_covering(
                    source,
                    media_id,
                    opts.audio_source_stream_index,
                    c.bitrate_bps,
                    start_secs,
                    start_secs + dur_secs,
                )
            })
            .await?;
        let mut attempt_opts = resolved.to_transcode_options();
        let mut timing = None;
        // A produced segment can be short of video frames while ffmpeg exits 0
        // (see `short_of_frames`). That is not transient — re-running the same
        // command reproduces it exactly — so the retry has to change something:
        // it seeds the decoder much further back, past the point the container
        // index lied about.
        let mut shortfall = None;
        // What ffmpeg reported the WINNING attempt produced: `(frames,
        // out_time_secs)`. Kept past the retry loop so the success path can
        // account for the frames rather than discarding the only evidence of
        // what was actually encoded.
        let mut reported: Option<(u64, f64)> = None;
        for attempt in 0..2 {
            if attempt == 1 {
                attempt_opts.decode_preroll_seconds =
                    Some(pharos_transcode::DECODE_PREROLL_RETRY_SECONDS);
            }
            timing = match self
                .write_segment(source, &attempt_opts, &tmp, class)
                .instrument(tracing::info_span!("write_segment"))
                .await
            {
                Ok(t) => t,
                Err(HlsCacheError::SchedulerBusy) => {
                    // Load-shedding, not breakage: the scheduler kept its
                    // remaining permits for work a client is blocked on. At
                    // ERROR this would drown the real failures it sits beside.
                    let _ = tokio::fs::remove_file(&tmp).await;
                    let _ = tokio::fs::remove_file(progress_sidecar_path(&tmp)).await;
                    tracing::debug!(
                        media.id = media_id,
                        seg = seg_index,
                        class = class.label(),
                        "hls segment shed: no spare transcode capacity"
                    );
                    record_segment_failure(SegmentOutcome::Shed, "scheduler_busy", class);
                    return Err(HlsCacheError::SchedulerBusy);
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    let _ = tokio::fs::remove_file(progress_sidecar_path(&tmp)).await;
                    // The success path below records twelve fields about a
                    // segment that WORKED and this path recorded none about one
                    // that did not: a failing segment reached the client as a
                    // bare 500 with no media id, no burn index, no device and no
                    // ffmpeg reason. That inversion is what made the
                    // browser-playback outage a guess — the only evidence a
                    // subtitle burn was failing was the proxy's 499s. Same
                    // dimensions as the success line, at ERROR.
                    tracing::error!(
                        media.id = media_id,
                        seg = seg_index,
                        attempt,
                        reason = failure_reason(&e),
                        error = %e,
                        codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
                        burn = opts.burn_subtitle_stream_index.is_some(),
                        burn_idx = opts.burn_subtitle_stream_index,
                        audio_idx = opts.audio_source_stream_index,
                        seek_secs = opts.window.start_seconds(),
                        preroll_secs = attempt_opts.decode_preroll_seconds,
                        source = %source.display(),
                        "hls segment transcode failed"
                    );
                    record_segment_failure(SegmentOutcome::Failed, failure_reason(&e), class);
                    return Err(e);
                }
            };
            // Read the `-progress` sidecar ONCE. It is consumed (deleted) on
            // read, and what ffmpeg reported it produced is needed twice: for
            // the gross completeness check here, and for the per-frame
            // accounting on the success path below.
            reported = read_progress(&tmp).await;
            shortfall = short_of_frames(reported, &attempt_opts);
            match &shortfall {
                None => break,
                Some(why) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    tracing::warn!(
                        media.id = media_id,
                        seg = seg_index,
                        attempt,
                        preroll_secs = attempt_opts.decode_preroll_seconds,
                        reason = %why,
                        "hls segment came back short of video frames"
                    );
                }
            }
        }
        if let Some(why) = shortfall {
            record_segment_outcome(SegmentOutcome::Short, class);
            return Err(HlsCacheError::Transcode(format!(
                "transcode produced an incomplete segment even with a \
                 {}s decode preroll: {why}",
                pharos_transcode::DECODE_PREROLL_RETRY_SECONDS
            )));
        }
        // Never CACHE an empty/truncated transcode. A worker can exit "success"
        // yet emit near-zero bytes (e.g. a hw encoder fed an option it rejects
        // produces a broken bitstream). Renaming that into the keyed cache path
        // poisons it: every later request serves the empty file in ~4 ms
        // forever (the truncated-fMP4 → empty-init → 500 loop seen live), and it
        // survives the underlying fix until manual eviction. Treat a sub-minimal
        // output as a transient transcode failure — leave the cache empty so the
        // next request re-attempts and a fixed encoder self-heals immediately.
        const MIN_SEGMENT_BYTES: u64 = 64;
        let produced = tokio::fs::metadata(&tmp)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if produced < MIN_SEGMENT_BYTES {
            let _ = tokio::fs::remove_file(&tmp).await;
            tracing::warn!(
                media.id = media_id,
                seg = seg_index,
                bytes = produced,
                codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
                "hls segment transcode produced empty/truncated output — not caching"
            );
            record_segment_outcome(SegmentOutcome::Empty, class);
            return Err(HlsCacheError::Transcode(format!(
                "transcode produced empty/truncated segment ({produced} bytes)"
            )));
        }
        tokio::fs::rename(&tmp, &path).await?;

        let bytes = tokio::fs::read(&path).await?;
        let transcode_ms = started.elapsed().as_millis();
        record_segment_outcome(SegmentOutcome::Ok, class);
        // Same label set as the hit arm so `sum by (result)` stays meaningful
        // and a hit-rate query does not have to special-case one side.
        // `hit_path` is "none" on a miss rather than absent: a label that
        // appears on only one arm of a ratio silently drops series.
        metrics::counter!(
            "pharos_segment_cache_total",
            "result" => "miss",
            "class" => class.label(),
            "hit_path" => "none",
        )
        .increment(1);
        // The same figure the log line carries, as a histogram: a segment
        // covers SEGMENT_SECONDS of playback, so a p95 above that means the
        // encoder is below realtime and clients are stalling. Answering that
        // from logs means trawling; this makes it a query.
        metrics::histogram!("pharos_segment_transcode_seconds")
            .record(started.elapsed().as_secs_f64());
        // The queue/encode split as queryable series, labelled by who was
        // waiting. `pharos_segment_transcode_seconds` says a segment was slow;
        // these say which half was slow and for whom. An interactive p95
        // queue-wait far above the encode p95 is the scheduler starving client
        // requests, not a slow encoder — the two need opposite fixes and were
        // indistinguishable from any metric before this.
        if let Some(t) = timing.as_ref() {
            metrics::histogram!(
                "pharos_transcode_queue_wait_seconds",
                "class" => class.label(),
            )
            .record(t.queue_wait_ms as f64 / 1000.0);
            metrics::histogram!(
                "pharos_transcode_encode_seconds",
                "class" => class.label(),
                "device" => t.device.to_string(),
            )
            .record(t.encode_ms as f64 / 1000.0);
            // What that encode was COMPETING WITH. `encode_seconds` rising is
            // ambiguous on a shared encoder — a heavier source and a crowded
            // device look identical — and the two need opposite fixes: a
            // cheaper ladder versus a scheduler that stops piling speculative
            // work onto the segment a client is blocked on. Measured on the
            // deployment before this shipped: 1 860 ms with no peers, 6 229 ms
            // with six. Paired with `encode_seconds` by the same labels so the
            // two divide.
            metrics::histogram!(
                "pharos_transcode_peer_jobs",
                "class" => class.label(),
                "device" => t.device.to_string(),
            )
            .record(t.peer_jobs as f64);
        }
        // Split total transcode_ms into scheduler queue-wait vs actual encode
        // (from the scheduler's JobDone), plus the winning device + retry count,
        // so a slow segment is diagnosable: high queue_wait_ms = saturated
        // devices / failed-device retry churn (e.g. phantom GPUs), high
        // encode_ms = a genuinely slow encoder. Fields land on the HTTP request
        // span this runs under.
        let seek_secs = opts.window.start_seconds();
        // Always known now that the window comes from the grid — it used to
        // be an Option because a caller could omit the duration.
        let seg_secs = opts.window.duration_seconds();
        // What the segment was SUPPOSED to contain, and what ffmpeg says it
        // did. The gross check above only rejects a segment that missed 10% of
        // its duration — at 6 s that is 600 ms, ~14 frames, a plainly visible
        // stutter that reads as a healthy segment. These fields make the
        // difference queryable instead of invisible.
        let expected_frames = opts.window.expected_frames();
        let produced_frames = reported.map(|(f, _)| f);
        let frame_deficit = match (expected_frames, produced_frames) {
            // Only meaningful when this segment actually encodes video.
            (Some(want), Some(got)) if opts.video.is_some() => Some(want as i64 - got as i64),
            _ => None,
        };
        tracing::info!(
            media.id = media_id,
            seg = seg_index,
            transcode_ms = transcode_ms as u64,
            // No `lock_wait_ms`: there is no lock to wait on. The wait a
            // requester can now suffer is the coalesce wait, and it is reported
            // where it is actually paid — `read_ms` on the
            // `hit_path="coalesced"` line, which the old field never covered
            // (the lock WINNER always logged ~0 and the waiters logged nothing).
            queue_wait_ms = timing.as_ref().map(|t| t.queue_wait_ms),
            encode_ms = timing.as_ref().map(|t| t.encode_ms),
            peer_jobs = timing.as_ref().map(|t| t.peer_jobs),
            device = timing.as_ref().map(|t| t.device.to_string()),
            bytes = bytes.len(),
            codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
            burn = opts.burn_subtitle_stream_index.is_some(),
            burn_idx = opts.burn_subtitle_stream_index,
            audio_idx = opts.audio_source_stream_index,
            seek_secs,
            expected_frames,
            produced_frames,
            frame_deficit,
            produced_secs = reported.map(|(_, s)| s),
            "hls segment transcoded (cache miss)"
        );
        // A segment short by even ONE frame is a visible hitch at its boundary,
        // and it is cached — so it replays identically on every later view.
        // Counted (bounded label: whether the source rate was known at all) and
        // warned, because a fault that only shows as a missing field on an INFO
        // line is a fault nobody queries for.
        if let Some(deficit) = frame_deficit {
            metrics::counter!(
                "pharos_segment_frames_total",
                "result" => if deficit > 0 { "short" } else { "complete" },
            )
            .increment(1);
            if deficit > 0 {
                tracing::warn!(
                    media.id = media_id,
                    seg = seg_index,
                    expected_frames,
                    produced_frames,
                    deficit,
                    seek_secs,
                    seg_secs,
                    device = timing.as_ref().map(|t| t.device.to_string()),
                    "hls segment is short of the frames its window implies — \
                     expect a hitch at this boundary"
                );
            }
        }
        // A segment covering N seconds of content that takes >3×N to encode
        // is drowning (client consumes 1×; even prefetch can't hide a 3×
        // deficit for long). Surface it at WARN with every dimension needed
        // to attribute the stall — the 170-225 s outliers observed live
        // (2026-07-14, Avatar burn path) were only findable by correlating
        // INFO lines after the fact.
        let realtime_budget_ms = seg_secs * 1000.0;
        if (transcode_ms as f64) > 3.0 * realtime_budget_ms {
            tracing::warn!(
                media.id = media_id,
                seg = seg_index,
                transcode_ms = transcode_ms as u64,
                queue_wait_ms = timing.as_ref().map(|t| t.queue_wait_ms),
                encode_ms = timing.as_ref().map(|t| t.encode_ms),
                device = timing.as_ref().map(|t| t.device.to_string()),
                codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
                burn = opts.burn_subtitle_stream_index.is_some(),
                seek_secs,
                seg_secs,
                source = %source.display(),
                "hls segment transcode far below realtime"
            );
        }
        self.record(key, bytes.len() as u64).await;
        self.maybe_evict().await;
        Ok(bytes)
    }

    #[cfg(test)]
    fn segment_path(&self, media_id: u64, seg_index: u32) -> PathBuf {
        self.segment_path_keyed(SegmentIdentity {
            media_id,
            seg_index,
            audio_index: 0,
            subtitle_index: NO_SUBTITLE,
            video_bitrate_kbps: 0,
            audio_bitrate_kbps: 0,
            codec_tag: 0,
        })
    }

    /// Compose `{root}/{media_id}/{seg}.ts` for the default case
    /// (audio=0, subtitle=-1, bitrate=auto) and a longer-form
    /// `{root}/{media_id}/{seg}-a{A}-s{S}-b{Bkbps}.ts` when any
    /// dimension diverges. Keeps the existing on-disk layout intact
    /// for warm caches that pre-date per-track + per-variant keys.
    fn segment_path_keyed(&self, key: SegmentIdentity) -> PathBuf {
        self.root.join(key.cache_relative_path())
    }

    /// Transcode one segment to `out`. Returns the scheduler's timing split
    /// (queue-wait vs encode + device) when the scheduler path ran, so the
    /// caller can attribute a slow segment; `None` on the inline fallback.
    async fn write_segment(
        &self,
        source: &Path,
        opts: &TranscodeOptions,
        out: &Path,
        class: JobClass,
    ) -> Result<Option<pharos_transcode::scheduler::JobDone>, HlsCacheError> {
        let _ = source.to_str().ok_or(HlsCacheError::NonUtf8Path)?;
        // Scheduler path: the worker writes the segment file itself,
        // load-balanced across GPUs + CPU. We just await completion.
        if let Some(sched) = &self.scheduler {
            use pharos_transcode::scheduler::SinkRequest;
            let done = sched
                .submit(
                    source.to_path_buf(),
                    opts.clone(),
                    SinkRequest::FileDirect {
                        out_path: out.to_path_buf(),
                    },
                    class,
                )
                .await
                .map_err(|e| match e {
                    pharos_transcode::scheduler::SchedError::Busy => HlsCacheError::SchedulerBusy,
                    other => HlsCacheError::Transcode(other.to_string()),
                })?;
            return Ok(Some(done));
        }
        // Legacy inline path: one ffmpeg, stream to file.
        let mut stream = self
            .transcoder
            .transcode(source, opts)
            .await
            .map_err(|e| HlsCacheError::Transcode(e.to_string()))?;
        let mut file = tokio::fs::File::create(out).await?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tokio::io::AsyncWriteExt::write_all(&mut file, &buf[..n]).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        // EOF on the pipe is not proof of success — a child killed mid-encode
        // closes stdout exactly like one that finished. Without this check a
        // truncated segment was renamed into the keyed cache path and served
        // from there forever. The scheduler path already gates on
        // `status.success()`; this is the same gate for the inline fallback.
        let status = stream
            .wait()
            .await
            .map_err(|e| HlsCacheError::Transcode(format!("reap ffmpeg: {e}")))?;
        if !status.success() {
            return Err(HlsCacheError::Transcode(format!(
                "ffmpeg exited unsuccessfully: {status}"
            )));
        }
        Ok(None)
    }

    /// A/V-sync fix (continuous-audio rendition): ensure a single ffmpeg is
    /// producing the whole audio track as an HLS rendition (fMP4 Opus, 6 s
    /// segments) into a per-(media,track,bitrate) directory, and return that
    /// directory. ONE continuous encode ⇒ one codec preskip total ⇒ gapless,
    /// driftless audio (vs the per-segment preskip that made audio creep ahead
    /// and click). The ffmpeg reads the source SEQUENTIALLY and produces
    /// segments far faster than realtime, so segment 0 appears almost
    /// immediately, with no multi-GB upfront read (the batch whole-file
    /// approach's fatal flaw).
    ///
    /// Idempotent + deduped: if the playlist already exists (a finished
    /// session) or one is mid-run, no new ffmpeg is spawned. The child is
    /// reaped by a detached task; kill-on-stop is a later optimization.
    pub async fn ensure_audio_hls(
        &self,
        source: &Path,
        media_id: u64,
        audio_index: Option<u32>,
        audio_bitrate_bps: Option<u64>,
    ) -> Result<PathBuf, HlsCacheError> {
        self.ensure_audio_hls_covering(source, media_id, audio_index, audio_bitrate_bps, 0)
            .await
    }

    /// How far past the newest written segment a request may point while we
    /// still just WAIT for the running from-behind session (it encodes many
    /// times realtime, so a small gap closes within the read poll budget).
    /// Anything further is a SEEK: spawn a second session at the target
    /// (B42 — the single from-0 session made deep seeks 404 "audio segment
    /// not ready" until the encoder crawled the whole file over NFS).
    const AUDIO_SEEK_LOOKAHEAD_SEGS: u32 = 20;

    /// Audio-rendition segment length. This is the ONE value `-hls_time` and
    /// the seek-session start position both read, because ffmpeg's HLS muxer
    /// cuts every `hls_time` seconds measured from the session's OWN first
    /// packet — so a session that starts anywhere other than a multiple of this
    /// produces boundaries no other session can reproduce.
    pub const AUDIO_SEGMENT_SECONDS: f64 = 6.0;

    /// Start time (seconds) of audio segment `seg`: the plain uniform grid.
    ///
    /// This deliberately does NOT frame-snap to the video's grid. The audio
    /// rendition is source-anchored (`-ss X` with a matching
    /// `-output_ts_offset X`, which cancel exactly), so every sample lands at
    /// its true source timestamp no matter where the session starts — the
    /// `-ss` value cannot create or fix A/V skew, it only decides which samples
    /// land in which FILE. Snapping it to the video frame grid therefore bought
    /// nothing and cost correctness: `-hls_time` cuts on this uniform grid, so a
    /// frame-snapped session cut its segments up to half a video frame away from
    /// where the from-0 session cut the same indices. Measured on a real 23.976
    /// source: `a5` began at 30.0065 s from the whole-file session and 29.9875 s
    /// from a seek session — the same filename, 19 ms apart.
    fn audio_seg_start_secs(seg: u32) -> f64 {
        seg as f64 * Self::AUDIO_SEGMENT_SECONDS
    }

    /// Directory the session starting at `start_seg` writes into: the rendition
    /// root for the whole-file (from-0) session, a private `s{start}` subdir for
    /// every SEEK session.
    ///
    /// Sessions cannot agree on segment boundaries — ffmpeg cuts relative to
    /// each session's own first packet, which after a seek lands at packet
    /// granularity rather than exactly on the grid — so two sessions writing
    /// `a{N}.m4s` into one directory produced two different files under one
    /// name, last writer winning. That made a segment's bytes change underneath
    /// a playing client at an arbitrary point mid-playback. Giving each session
    /// its own directory and resolving reads deterministically confines the
    /// residual (~20 ms, one audio packet) mismatch to the seek point itself,
    /// where the audio is discontinuous anyway.
    fn audio_session_dir(root: &Path, start_seg: u32) -> PathBuf {
        if start_seg == 0 {
            root.to_path_buf()
        } else {
            root.join(format!("s{start_seg}"))
        }
    }

    /// Every session start present under a rendition root, deepest first. The
    /// from-0 session (0) is always a candidate; seek sessions announce
    /// themselves by their `s{start}` directory.
    async fn audio_session_starts(root: &Path) -> Vec<u32> {
        let mut starts = vec![0u32];
        if let Ok(mut rd) = tokio::fs::read_dir(root).await {
            while let Ok(Some(e)) = rd.next_entry().await {
                if let Some(n) = e
                    .file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix('s'))
                    .and_then(|r| r.parse::<u32>().ok())
                {
                    starts.push(n);
                }
            }
        }
        starts.sort_unstable_by(|a, b| b.cmp(a));
        starts.dedup();
        starts
    }

    /// Locate one produced file across a rendition's sessions.
    ///
    /// For a media segment `a{N}.m4s` the answer is the DEEPEST session whose
    /// start is `<= N` and which has actually written it — so a client playing
    /// on from a seek keeps drawing from that one session for every subsequent
    /// segment instead of alternating with the whole-file session as it catches
    /// up. Non-segment names (`init.mp4`) take the first session that has one;
    /// the init is codec configuration and is identical across sessions.
    /// Returns the file AND the segment its session started at — the caller
    /// needs that to place the fragment on the timeline (see B121: ffmpeg
    /// numbers a session's `tfdt` from ITS OWN first fragment, not from the
    /// absolute grid).
    async fn resolve_audio_file(root: &Path, name: &str) -> Option<(PathBuf, u32)> {
        let want = name
            .strip_prefix('a')
            .and_then(|r| r.strip_suffix(".m4s"))
            .and_then(|r| r.parse::<u32>().ok());
        for start in Self::audio_session_starts(root).await {
            if want.is_some_and(|w| start > w) {
                continue;
            }
            let p = Self::audio_session_dir(root, start).join(name);
            if tokio::fs::try_exists(&p).await.unwrap_or(false) {
                return Some((p, start));
            }
        }
        None
    }

    /// Decide which audio-rendition session serves `want_seg`. Pure so the
    /// slow-swap / seek-coalescing policy is unit-testable without touching the
    /// filesystem or spawning ffmpeg.
    ///
    /// - `from0_active`: a whole-file from-0 session is running or finished.
    /// - `seek_progress`: highest segment index any running session has written.
    ///
    /// A fresh mid-file audio-track switch (new `-a{idx}` dir: no from-0
    /// session, no progress) seeks straight to the playhead instead of the old
    /// `want_seg <= LOOKAHEAD => 0` rule, which re-encoded 0→playhead over NFS
    /// first — the "incredibly long swap" symptom (B106).
    fn choose_audio_start_seg(
        want_seg: u32,
        from0_active: bool,
        seek_progress: Option<u32>,
    ) -> AudioStart {
        // A from-0 session writes sequentially from 0, so it promptly covers
        // only the near-start window — reuse it there rather than spawning a
        // redundant seek session during ordinary early sequential play.
        if from0_active && want_seg <= Self::AUDIO_SEEK_LOOKAHEAD_SEGS {
            return AudioStart::Reuse;
        }
        // A running session has written up to `n_max`; a forward target within
        // the lookahead window lands during the read poll.
        if let Some(n_max) = seek_progress {
            if want_seg >= n_max
                && want_seg <= n_max.saturating_add(Self::AUDIO_SEEK_LOOKAHEAD_SEGS)
            {
                return AudioStart::Reuse;
            }
        }
        // Otherwise start a session AT the playhead. Only a genuine
        // start-of-file request uses the whole-file from-0 rendition.
        AudioStart::Start(want_seg)
    }

    /// Ensure an audio-rendition session exists whose output will cover
    /// `want_seg` promptly. `want_seg == 0` is the plain from-the-start
    /// session; a deep target spawns an additional session seeked to that
    /// segment boundary (`-ss`, `-start_number`, `-output_ts_offset` so the
    /// fmp4 timestamps stay source-anchored). Each session writes into its own
    /// directory (see [`audio_session_dir`](Self::audio_session_dir)) because
    /// they cannot agree on where a boundary falls; reads resolve across them
    /// via [`resolve_audio_file`](Self::resolve_audio_file).
    pub async fn ensure_audio_hls_covering(
        &self,
        source: &Path,
        media_id: u64,
        audio_index: Option<u32>,
        audio_bitrate_bps: Option<u64>,
        want_seg: u32,
    ) -> Result<PathBuf, HlsCacheError> {
        let a = audio_index.unwrap_or(0);
        let br = audio_bitrate_bps.map(|b| b / 1000).unwrap_or(0);
        let dir = self
            .root
            .join("_audiohls")
            .join(format!("{media_id}-a{a}-b{br}"));
        let playlist = dir.join("audio.m3u8");
        // The requested segment already exists in SOME session → nothing to
        // spawn.
        if Self::resolve_audio_file(&dir, &format!("a{want_seg}.m4s"))
            .await
            .is_some()
        {
            return Ok(dir);
        }
        // Pick the session start that serves this request. A from-0 session
        // (running or finished) covers the near-start window; deeper — or a
        // fresh mid-file audio-track switch — seeks straight to the playhead
        // rather than re-encoding 0→playhead first (B106 slow-swap fix).
        let from0_active = tokio::fs::try_exists(&playlist).await.unwrap_or(false)
            || tokio::fs::try_exists(&dir.join(".running"))
                .await
                .unwrap_or(false);
        let progress = Self::audio_session_progress(&dir).await;
        let start_seg = match Self::choose_audio_start_seg(want_seg, from0_active, progress) {
            AudioStart::Reuse => return Ok(dir),
            AudioStart::Start(s) => s,
        };
        let running = dir.join(if start_seg == 0 {
            ".running".to_string()
        } else {
            format!(".running-{start_seg}")
        });
        // Already finished (from-0 leaves the playlist as its done-marker),
        // or a session for this start is in flight → reuse.
        if (start_seg == 0 && tokio::fs::try_exists(&playlist).await.unwrap_or(false))
            || tokio::fs::try_exists(&running).await.unwrap_or(false)
        {
            return Ok(dir);
        }
        let lock = {
            let mut state = self.state.lock().await;
            state
                .audio_locks
                .entry(running.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        // Re-check under the lock.
        if (start_seg == 0 && tokio::fs::try_exists(&playlist).await.unwrap_or(false))
            || tokio::fs::try_exists(&running).await.unwrap_or(false)
        {
            return Ok(dir);
        }
        tokio::fs::create_dir_all(Self::audio_session_dir(&dir, start_seg)).await?;
        tokio::fs::write(&running, b"").await?;

        let audio_start = self.source_audio_start(source, audio_index).await;
        let args = Self::audio_hls_args(
            source,
            &dir,
            audio_index,
            audio_bitrate_bps,
            start_seg,
            audio_start,
        )?;
        if start_seg > 0 {
            tracing::info!(
                media.id = media_id,
                start_seg,
                "audio HLS: spawning seek session (B42)"
            );
        }

        let bin = self.transcoder.binary().to_path_buf();
        let running_marker = running.clone();
        let media = media_id;
        // Detached: run the encode to completion, then drop the `.running`
        // marker (the from-0 session leaves `audio.m3u8` as the done-marker).
        tokio::spawn(async move {
            let mut cmd = tokio::process::Command::new(&bin);
            cmd.args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            match cmd.spawn() {
                Ok(mut child) => {
                    let status = child.wait().await;
                    if let Ok(s) = status {
                        if !s.success() {
                            tracing::warn!(
                                media.id = media,
                                ?s,
                                "audio HLS session exited non-zero"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(media.id = media, error = %e, "failed to spawn audio HLS session");
                }
            }
            let _ = tokio::fs::remove_file(&running_marker).await;
        });
        Ok(dir)
    }

    /// Highest `aN.m4s` index written by ANY session of this rendition — the
    /// overall write progress. `None` when no segment exists yet. Must span the
    /// per-session subdirectories, or a seek session's output is invisible to
    /// the progress-aware read wait and to `choose_audio_start_seg`.
    async fn audio_session_progress(root: &Path) -> Option<u32> {
        let mut best: Option<u32> = None;
        for start in Self::audio_session_starts(root).await {
            let dir = Self::audio_session_dir(root, start);
            let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(e)) = rd.next_entry().await {
                if let Some(n) = e
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_prefix('a'))
                    .and_then(|r| r.strip_suffix(".m4s"))
                    .and_then(|r| r.parse::<u32>().ok())
                {
                    best = Some(best.map_or(n, |b| b.max(n)));
                }
            }
        }
        best
    }

    /// The TRUE source position of a continuous-audio file's first sample.
    ///
    /// A session records the start it was ASKED for, but that is not always
    /// where its audio begins: for a source whose audio stream starts late, a
    /// from-0 session's first sample sits at the stream's own start, not at 0.
    /// The segment slice seeks this file by `input_seek - start_seconds`, and
    /// `-ss` on an input is measured from that input's own start time, so a
    /// wrong `start_seconds` puts the audio under the wrong video by exactly
    /// the difference — measured at +1680 ms on a title whose audio stream
    /// starts at 1.700 s, heard as audio running ahead of picture.
    ///
    /// Probed once per session file and cached: the value cannot change once
    /// the file exists. Falls back to the requested start when the file cannot
    /// be probed yet, which is the previous behaviour and no worse.
    async fn probed_session_start(&self, path: &Path, requested: f64) -> f64 {
        if let Some(v) = self
            .state
            .lock()
            .await
            .audio_session_starts
            .get(path)
            .copied()
        {
            return v;
        }
        let Some(actual) = Self::first_audio_pts(self.transcoder.binary(), path).await else {
            return requested;
        };
        // Only ever LATER than requested: a session cannot begin before the
        // point it was seeked to, and a small negative would be muxer jitter.
        let resolved = if actual > requested {
            actual
        } else {
            requested
        };
        if (resolved - requested).abs() > 0.05 {
            tracing::info!(
                audio.session = %path.display(),
                requested_start = requested,
                actual_start = actual,
                skew_ms = ((actual - requested) * 1000.0) as i64,
                "continuous audio session does not start where it was asked to — \
                 slicing it by the requested start would shift the audio"
            );
        }
        self.state
            .lock()
            .await
            .audio_session_starts
            .insert(path.to_path_buf(), resolved);
        resolved
    }

    /// Where the SOURCE's chosen audio track begins, in seconds.
    ///
    /// Zero for almost every file. When it is not — a track authored to start
    /// after the video — every encode of that track inherits the offset, and
    /// anything that assumes "audio time == video time" is wrong by exactly
    /// this much (B119 on the muxed path, B120 on the demuxed rendition).
    /// Probed once per `(source, track)`; it cannot change while the file does
    /// not. An unprobeable source reports 0.0, which is the old behaviour.
    async fn source_audio_start(&self, source: &Path, audio_index: Option<u32>) -> f64 {
        let idx = audio_index.unwrap_or(0);
        let key = (source.to_path_buf(), idx);
        if let Some(v) = self
            .state
            .lock()
            .await
            .source_audio_starts
            .get(&key)
            .copied()
        {
            return v;
        }
        let start = Self::audio_stream_start(self.transcoder.binary(), source, idx)
            .await
            .unwrap_or(0.0)
            .max(0.0);
        if start > 0.0 {
            tracing::info!(
                source = %source.display(),
                audio_index = idx,
                audio_start_secs = start,
                "source audio track starts after its video — encodes of it are \
                 padded to keep the source timeline"
            );
        }
        self.state
            .lock()
            .await
            .source_audio_starts
            .insert(key, start);
        start
    }

    /// `start_time` of the `a:{index}` stream of `path`, in seconds.
    async fn audio_stream_start(ffmpeg_bin: &Path, path: &Path, index: u32) -> Option<f64> {
        let probe = ffmpeg_bin.with_file_name("ffprobe");
        let out = tokio::process::Command::new(&probe)
            .args([
                "-v",
                "error",
                "-select_streams",
                &format!("a:{index}"),
                "-show_entries",
                "stream=start_time",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .await
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<f64>().ok())
    }

    /// PTS of the first audio packet in `path`, in seconds.
    async fn first_audio_pts(ffmpeg_bin: &Path, path: &Path) -> Option<f64> {
        let probe = ffmpeg_bin.with_file_name("ffprobe");
        let out = tokio::process::Command::new(&probe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "packet=pts_time",
                "-of",
                "csv=p=0",
                "-read_intervals",
                "%+#1",
            ])
            .arg(path)
            .output()
            .await
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|l| l.split(',').next())
            .and_then(|v| v.trim().parse::<f64>().ok())
    }

    /// Highest `aN.m4s` index written by one session directory.
    async fn session_dir_progress(dir: &Path) -> Option<u32> {
        let mut best: Option<u32> = None;
        let mut rd = tokio::fs::read_dir(dir).await.ok()?;
        while let Ok(Some(e)) = rd.next_entry().await {
            if let Some(n) = e
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix('a'))
                .and_then(|r| r.strip_suffix(".m4s"))
                .and_then(|r| r.parse::<u32>().ok())
            {
                best = Some(best.map_or(n, |b| b.max(n)));
            }
        }
        best
    }

    /// Write progress AS THE READ WAIT SHOULD SEE IT for `name`: the progress
    /// of the sessions that could actually serve this request.
    ///
    /// [`audio_session_progress`](Self::audio_session_progress) is a global
    /// high-water mark across every session under the rendition root, which
    /// makes the wait answer the wrong question twice over. A session seeked
    /// PAST the target can never write it, yet its advance kept the wait alive
    /// to the full 30 s cap; and — the failure that matters — once ANY session
    /// had produced anything, a freshly-spawned seek session was denied the
    /// cold-start grace and judged on the much shorter stall budget while it
    /// was still opening the source over NFS.
    ///
    /// So: consider only sessions whose start is `<= want` (mirroring
    /// [`resolve_audio_file`](Self::resolve_audio_file)'s selection), and report
    /// `None` — "not started" — while the deepest of them, the one spawned to
    /// serve this very request, has written nothing.
    async fn audio_wait_progress(root: &Path, name: &str) -> Option<u32> {
        let Some(want) = name
            .strip_prefix('a')
            .and_then(|r| r.strip_suffix(".m4s"))
            .and_then(|r| r.parse::<u32>().ok())
        else {
            // `init.mp4` / `audio.m3u8` are not on the segment timeline; any
            // session's output answers them.
            return Self::audio_session_progress(root).await;
        };
        let candidates: Vec<u32> = Self::audio_session_starts(root)
            .await
            .into_iter()
            .filter(|s| *s <= want)
            .collect();
        // `audio_session_starts` returns deepest first.
        let deepest = *candidates.first()?;
        Self::session_dir_progress(&Self::audio_session_dir(root, deepest)).await?;
        let mut best: Option<u32> = None;
        for start in candidates {
            if let Some(n) = Self::session_dir_progress(&Self::audio_session_dir(root, start)).await
            {
                best = Some(best.map_or(n, |b: u32| b.max(n)));
            }
        }
        best
    }

    /// Build the ffmpeg argv for an audio-rendition session starting at
    /// `start_seg` (0 = whole file). Seek sessions are source-anchored:
    /// `-ss` input seek to the segment boundary, `-start_number` so the
    /// emitted names line up with the absolute segment index, and
    /// `-output_ts_offset` so each fragment's tfdt carries its true timeline
    /// position (a PTS-0 fragment would buffer at 0:00 in hls.js — the same
    /// failure class as B41's mpegts segments).
    fn audio_hls_args(
        source: &Path,
        root: &Path,
        audio_index: Option<u32>,
        audio_bitrate_bps: Option<u64>,
        start_seg: u32,
        audio_start_secs: f64,
    ) -> Result<Vec<String>, HlsCacheError> {
        let src = source
            .to_str()
            .ok_or(HlsCacheError::NonUtf8Path)?
            .to_string();
        // Each session owns its output directory (see `audio_session_dir`), so
        // two sessions can never write two different files under one name.
        let dir = Self::audio_session_dir(root, start_seg);
        let seg_pat = dir
            .join("a%d.m4s")
            .to_str()
            .ok_or(HlsCacheError::NonUtf8Path)?
            .to_string();
        // `audio.m3u8` at the rendition ROOT doubles as the from-0 session's
        // done-marker. A seek session writes its own inside its own directory,
        // so it cannot clobber that marker.
        let m3u8 = dir
            .join("audio.m3u8")
            .to_str()
            .ok_or(HlsCacheError::NonUtf8Path)?
            .to_string();
        let bitrate = audio_bitrate_bps.unwrap_or(128_000);
        let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        // Seek to the same uniform grid `-hls_time` below cuts on, so a seek
        // session's boundaries land where the whole-file session's would (see
        // `audio_seg_start_secs`). Six decimals, not three: the millisecond
        // rounding this used to apply is coarser than an audio packet.
        let start_secs = Self::audio_seg_start_secs(start_seg);
        if start_seg > 0 {
            args.push("-ss".into());
            args.push(format!("{start_secs:.6}"));
        }
        args.push("-i".into());
        args.push(src);
        args.push("-vn".into());
        // Explicit track select when the client picked one; else ffmpeg default.
        if let Some(idx) = audio_index {
            args.push("-map".into());
            args.push(format!("0:a:{idx}"));
        } else {
            args.push("-map".into());
            args.push("0:a:0?".into());
        }
        // A from-0 session must carry the audio stream's OWN start time, or its
        // fragments hold content from `audio_start_secs` later than the grid
        // position they are served at.
        //
        // ffmpeg re-bases the only mapped stream so its first sample sits at
        // zero, and for a track that begins after the video (Rogue One: eac3 at
        // 1.700 s against video at 0) that silently discards the offset: a17
        // then carried source 103.700 s while the playlist placed it at
        // 101.997 s — audio 1.7 s AHEAD of picture for the whole title.
        // `-output_ts_offset`, `-copyts` and `-avoid_negative_ts disabled` all
        // leave it at zero (measured, ffmpeg 8.1); padding the front is what
        // actually restores the source timeline. A SEEK session needs no pad —
        // `-ss X` lands on source X exactly.
        if start_seg == 0 && audio_start_secs > 0.0 {
            args.push("-af".into());
            args.push(format!(
                "adelay=delays={:.3}ms:all=1",
                audio_start_secs * 1000.0
            ));
        }
        args.extend(
            ["-c:a", "libopus", "-b:a", &bitrate.to_string(), "-ac", "2"]
                .into_iter()
                .map(String::from),
        );
        // Exactly cancels the `-ss` above, so every sample keeps its true source
        // timestamp and the rendition stays anchored regardless of where the
        // session started. Must use the same precision as the `-ss` or the two
        // no longer cancel.
        if start_seg > 0 {
            args.push("-output_ts_offset".into());
            args.push(format!("{start_secs:.6}"));
        }
        args.extend(
            [
                "-f",
                "hls",
                "-hls_time",
                &format!("{}", Self::AUDIO_SEGMENT_SECONDS),
                "-hls_segment_type",
                "fmp4",
                "-hls_playlist_type",
                "vod",
                "-hls_flags",
                "independent_segments",
                "-hls_fmp4_init_filename",
                "init.mp4",
                "-hls_list_size",
                "0",
            ]
            .into_iter()
            .map(String::from),
        );
        if start_seg > 0 {
            args.push("-start_number".into());
            args.push(start_seg.to_string());
        }
        args.push("-hls_segment_filename".into());
        args.push(seg_pat);
        args.push(m3u8);
        Ok(args)
    }

    // ---- Continuous AAC encode, for segments that MUX their audio ----------
    //
    // The mpegts surface (native / Google TV) muxes audio into each media
    // segment. Encoding that audio per segment re-primes the AAC encoder at
    // every boundary: each segment emits its own priming frame and starts a
    // fresh frame grid at its own seek point, so consecutive segments carry
    // overlapping, phase-misaligned audio — the same instants encoded twice,
    // at different timestamps, from different encoder state. Measured live on
    // Fringe S01E02: 6.037 s of audio against 6.006 s of video in every
    // segment, and the copies 0.53 of a frame out of phase with each other.
    //
    // The fix is one encode per title, sliced by copy. This produces that
    // encode; the segment ffmpeg takes it as a second input and `-c:a copy`s
    // the slice, so every audio frame belongs to exactly one segment at a
    // deterministic PTS and there is only one grid to drift from.
    //
    // MPEG-TS rather than ADTS or a growing MP4: it is self-framing AND
    // carries absolute PTS, so the segment's `-ss` lands on the global grid.

    /// Where the continuous encode for one `(media, track, bitrate)` lives.
    fn continuous_audio_root(
        &self,
        media_id: u64,
        audio_index: Option<u32>,
        audio_bitrate_bps: Option<u64>,
    ) -> PathBuf {
        let a = audio_index.unwrap_or(0);
        let br = audio_bitrate_bps.map(|b| b / 1000).unwrap_or(0);
        self.root
            .join("_contaudio")
            .join(format!("{media_id}-a{a}-b{br}"))
    }

    /// Start position of the session that can serve a segment starting at
    /// `want_start_secs`.
    ///
    /// The segment's ffmpeg seeks BOTH inputs to the same point — one decode
    /// preroll before the segment start — because two inputs seeked to
    /// different positions are re-based by different amounts and the audio
    /// would land offset from the video by the difference. So the continuous
    /// encode has to reach back at least that far, not merely to the segment
    /// start.
    ///
    /// Snapped down to the audio grid so that repeated requests around one
    /// playhead resolve to the SAME session instead of spawning a new encode
    /// per segment.
    fn continuous_audio_session_start_secs(want_start_secs: f64) -> f64 {
        let earliest = want_start_secs - pharos_transcode::DECODE_PREROLL_SECONDS;
        if earliest <= 0.0 {
            return 0.0;
        }
        (earliest / Self::AUDIO_SEGMENT_SECONDS).floor() * Self::AUDIO_SEGMENT_SECONDS
    }

    /// Session directory for a continuous encode starting at `start_secs`.
    /// Whole-file sessions own the root; every seek session gets its own,
    /// for the same reason the rendition sessions do.
    fn continuous_audio_session_dir(root: &Path, start_secs: f64) -> PathBuf {
        if start_secs <= 0.0 {
            root.to_path_buf()
        } else {
            root.join(format!("s{}", start_secs as u64))
        }
    }

    /// ffmpeg argv for one continuous encode. Source-anchored exactly as the
    /// rendition sessions are: `-ss X` with a matching `-output_ts_offset X`,
    /// which cancel, so every sample keeps its true source timestamp and a
    /// seek session's output is interchangeable with the whole-file one.
    fn continuous_audio_args(
        source: &Path,
        dir: &Path,
        audio_index: Option<u32>,
        audio_bitrate_bps: Option<u64>,
        start_secs: f64,
    ) -> Result<Vec<String>, HlsCacheError> {
        let src = source.to_str().ok_or(HlsCacheError::NonUtf8Path)?;
        let out = dir.join("audio.ts");
        let out_s = out.to_str().ok_or(HlsCacheError::NonUtf8Path)?;
        let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        if start_secs > 0.0 {
            args.push("-ss".into());
            args.push(format!("{start_secs:.6}"));
        }
        args.push("-i".into());
        args.push(src.into());
        args.push("-vn".into());
        args.push("-map".into());
        args.push(match audio_index {
            Some(i) => format!("0:a:{i}"),
            None => "0:a:0?".into(),
        });
        args.extend(
            [
                "-c:a",
                "aac",
                "-b:a",
                &audio_bitrate_bps.unwrap_or(128_000).to_string(),
                "-ac",
                "2",
            ]
            .into_iter()
            .map(String::from),
        );
        if start_secs > 0.0 {
            args.push("-output_ts_offset".into());
            args.push(format!("{start_secs:.6}"));
        }
        // The mpegts muxer's default 1.4 s initial cue delay would shift every
        // sample away from its source timestamp, which is the one thing this
        // encode exists to preserve.
        args.extend(
            ["-f", "mpegts", "-muxdelay", "0", "-muxpreload", "0"]
                .into_iter()
                .map(String::from),
        );
        // How far the encode has got, so a reader can wait for coverage
        // instead of probing a file that is still being written.
        args.push("-progress".into());
        args.push(
            pharos_transcode::progress_sidecar_path(&out)
                .to_string_lossy()
                .into_owned(),
        );
        args.push("-y".into());
        args.push(out_s.into());
        Ok(args)
    }

    /// Source position the session starting at `start_secs` has encoded up to,
    /// read from its progress report. `None` while it has produced nothing.
    ///
    /// `out_time` counts from the session's own first sample and does NOT
    /// include `-output_ts_offset` (verified against ffmpeg 8.1), so the
    /// covered position is the session start plus it.
    async fn continuous_audio_covered_secs(dir: &Path, start_secs: f64) -> Option<f64> {
        let report = tokio::fs::read_to_string(pharos_transcode::progress_sidecar_path(
            &dir.join("audio.ts"),
        ))
        .await
        .ok()?;
        let out_time = report
            .lines()
            .filter_map(|l| l.strip_prefix("out_time_us="))
            .filter_map(|v| v.trim().parse::<f64>().ok())
            .next_back()?;
        Some(start_secs + out_time / 1e6)
    }

    /// Ensure a continuous AAC encode exists covering `[want_start_secs,
    /// want_end_secs)` and return the file a segment should `-c:a copy` from.
    ///
    /// Blocks until the encode has reached `want_end_secs`, or until the
    /// session that was producing it exits — an encode that ran to completion
    /// without reaching the request has hit the end of the source, and its
    /// output is as complete as it will ever be.
    pub async fn ensure_continuous_audio_covering(
        &self,
        source: &Path,
        media_id: u64,
        audio_index: Option<u32>,
        audio_bitrate_bps: Option<u64>,
        want_start_secs: f64,
        want_end_secs: f64,
    ) -> Result<pharos_transcode::MuxedAudio, HlsCacheError> {
        let root = self.continuous_audio_root(media_id, audio_index, audio_bitrate_bps);
        let start_secs = Self::continuous_audio_session_start_secs(want_start_secs);
        // A whole-file session that has already got this far serves the
        // request without spawning anything; prefer it so ordinary sequential
        // play does not accumulate one encode per seek.
        for candidate in [0.0, start_secs] {
            if candidate > want_start_secs {
                continue;
            }
            let dir = Self::continuous_audio_session_dir(&root, candidate);
            if Self::continuous_audio_covered_secs(&dir, candidate)
                .await
                .is_some_and(|c| c >= want_end_secs)
            {
                // The file EXISTS and is covered, so its true first sample is
                // knowable — take that over the start it was asked for.
                let path = dir.join("audio.ts");
                let start_seconds = self.probed_session_start(&path, candidate).await;
                return Ok(pharos_transcode::MuxedAudio {
                    path,
                    start_seconds,
                });
            }
        }

        let dir = Self::continuous_audio_session_dir(&root, start_secs);
        let file = dir.join("audio.ts");
        // The session start travels WITH the path: a segment has to seek this
        // file relative to its own first sample, and cannot do that from the
        // path alone.
        let produced = pharos_transcode::MuxedAudio {
            path: file.clone(),
            start_seconds: start_secs,
        };
        let running = root.join(format!(".running-cont-{}", start_secs as u64));
        let lock = {
            let mut state = self.state.lock().await;
            state
                .audio_locks
                .entry(running.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let spawned = {
            let _guard = lock.lock().await;
            if tokio::fs::try_exists(&running).await.unwrap_or(false) {
                false
            } else {
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(&running, b"").await?;
                let args = Self::continuous_audio_args(
                    source,
                    &dir,
                    audio_index,
                    audio_bitrate_bps,
                    start_secs,
                )?;
                let bin = self.transcoder.binary().to_path_buf();
                let marker = running.clone();
                tracing::info!(
                    media.id = media_id,
                    start_secs,
                    "continuous audio: spawning AAC encode for muxed segments"
                );
                // Whole-title vs seek session. Worth separating: the two
                // differ in whether the encode's own start time is zero, and
                // a bug that only appears on seek sessions is invisible in
                // aggregate — one shipped, putting unrelated audio under
                // correct video for anyone past the first few minutes.
                metrics::counter!(
                    "pharos_continuous_audio_sessions_total",
                    "start" => if start_secs > 0.0 { "seek" } else { "from_zero" },
                )
                .increment(1);
                tokio::spawn(async move {
                    let mut cmd = tokio::process::Command::new(&bin);
                    cmd.args(&args)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    match cmd.spawn() {
                        Ok(mut child) => {
                            if let Ok(s) = child.wait().await {
                                if !s.success() {
                                    tracing::warn!(
                                        media.id = media_id,
                                        ?s,
                                        "continuous audio encode exited non-zero"
                                    );
                                }
                            }
                        }
                        Err(e) => tracing::warn!(
                            media.id = media_id,
                            error = %e,
                            "failed to spawn continuous audio encode"
                        ),
                    }
                    let _ = tokio::fs::remove_file(&marker).await;
                });
                true
            }
        };
        let _ = spawned;

        for _ in 0..Self::AUDIO_POLL_MAX {
            if Self::continuous_audio_covered_secs(&dir, start_secs)
                .await
                .is_some_and(|c| c >= want_end_secs)
            {
                return Ok(pharos_transcode::MuxedAudio {
                    start_seconds: self.probed_session_start(&file, start_secs).await,
                    ..produced
                });
            }
            // The encode finished without reaching the request: it ran out of
            // source. Whatever it wrote is final, so stop waiting for more.
            if !tokio::fs::try_exists(&running).await.unwrap_or(false)
                && tokio::fs::try_exists(&file).await.unwrap_or(false)
            {
                return Ok(pharos_transcode::MuxedAudio {
                    start_seconds: self.probed_session_start(&file, start_secs).await,
                    ..produced
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                Self::AUDIO_POLL_INTERVAL_MS,
            ))
            .await;
        }
        Err(HlsCacheError::Transcode(format!(
            "continuous audio encode did not reach {want_end_secs:.3}s for media {media_id}"
        )))
    }

    /// Poll interval + budgets for [`audio_hls_file`](Self::audio_hls_file).
    /// The old flat "100 × 50 ms = 5 s then 404" gave up while a cold session
    /// was STILL PRODUCING: a deep seek spawns an ffmpeg that must open the
    /// whole source over NFS and encode opus to the target segment, which can
    /// exceed 5 s — the client then got a spurious 404 "audio segment not
    /// ready" and hls.js stalled the seek (the high-severity VP9 finding).
    const AUDIO_POLL_INTERVAL_MS: u64 = 50;
    /// Overall hard cap (× interval) — 30 s, so a very deep cold seek still has
    /// room even on slow storage.
    const AUDIO_POLL_MAX: usize = 600;
    /// Give up this many polls (12 s) after the session has produced NOTHING at
    /// all — the ffmpeg failed to start or died before its first segment.
    const AUDIO_POLL_NO_PROGRESS: usize = 240;
    /// Give up this many polls (10 s) after a session that WAS producing stops
    /// advancing — it finished (target genuinely absent) or wedged.
    ///
    /// This is an inter-segment interval, not a latency budget: a continuous
    /// audio session emits one 6 s segment at a time, so at 2× realtime it
    /// writes one every 3 s. The previous 3 s sat exactly on that cliff, and
    /// any I/O contention pushed a HEALTHY session over it. Ghost in the Shell
    /// (2026-07-25) did precisely that — the doubled video ladder saturated the
    /// same NFS stream, the audio session fell below one segment per 3 s, and
    /// the read declared a live session wedged, 404ing `a5.m4s` and `a48.m4s`
    /// so hls.js stalled. A false 404 breaks playback; a slow 404 on a
    /// genuinely-absent segment only costs latency, and the 30 s
    /// `AUDIO_POLL_MAX` still bounds it.
    const AUDIO_POLL_STALL: usize = 200;

    /// Read a produced audio-rendition file (`init.mp4`, `aN.m4s`, or
    /// `audio.m3u8`) from an [`ensure_audio_hls`](Self::ensure_audio_hls)
    /// directory, waiting for the continuous ffmpeg to produce it. Waits WHILE
    /// the session keeps writing new segments (progress advancing), and gives up
    /// only when the session stalls or never starts — so a slow-but-progressing
    /// cold seek is served instead of a false 404, while a dead session still
    /// fails promptly. Returns `NotFound` past the budget.
    pub async fn audio_hls_file(
        &self,
        dir: &Path,
        name: &str,
    ) -> Result<AudioRenditionFile, HlsCacheError> {
        self.audio_hls_file_budget(
            dir,
            name,
            Self::AUDIO_POLL_MAX,
            Self::AUDIO_POLL_NO_PROGRESS,
            Self::AUDIO_POLL_STALL,
        )
        .await
    }

    /// Budget-parameterised core of [`audio_hls_file`](Self::audio_hls_file), so
    /// the progress-aware wait is unit-testable without real 30 s timeouts.
    async fn audio_hls_file_budget(
        &self,
        dir: &Path,
        name: &str,
        max_polls: usize,
        no_progress_polls: usize,
        stall_polls: usize,
    ) -> Result<AudioRenditionFile, HlsCacheError> {
        // Basic traversal guard: names are simple file basenames.
        if name.contains('/') || name.contains("..") {
            return Err(HlsCacheError::Io(std::io::Error::from(
                std::io::ErrorKind::InvalidInput,
            )));
        }
        let mut last_progress: Option<u32> = None;
        let mut stalls = 0usize;
        let mut polls = 0usize;
        let mut give_up = AudioWaitGiveUp::BudgetExhausted;
        for i in 0..max_polls {
            polls = i;
            // Resolve across the rendition's sessions each poll: the file may
            // not exist yet, and which session ends up owning it is only known
            // once one has written it.
            if let Some((path, session_start_seg)) = Self::resolve_audio_file(dir, name).await {
                if let Ok(b) = tokio::fs::read(&path).await {
                    if !b.is_empty() {
                        record_audio_wait("served");
                        return Ok(AudioRenditionFile {
                            bytes: b,
                            session_start_seg,
                        });
                    }
                }
            }
            match Self::audio_wait_progress(dir, name).await {
                // The session has written at least one segment. Wait while it
                // keeps advancing toward our target; give up once it stalls.
                Some(prog) => {
                    if Some(prog) == last_progress {
                        stalls += 1;
                        if stalls >= stall_polls {
                            give_up = AudioWaitGiveUp::Stalled;
                            break;
                        }
                    } else {
                        stalls = 0;
                        last_progress = Some(prog);
                    }
                }
                // Nothing produced yet — a cold NFS open before the first
                // segment. Allow a bounded grace, then declare the session dead.
                None => {
                    if i >= no_progress_polls {
                        give_up = AudioWaitGiveUp::NeverStarted;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                Self::AUDIO_POLL_INTERVAL_MS,
            ))
            .await;
        }
        let waited_ms = polls as u64 * Self::AUDIO_POLL_INTERVAL_MS;
        record_audio_wait(give_up.label());
        tracing::warn!(
            audio.file = name,
            audio.dir = %dir.display(),
            reason = give_up.label(),
            waited_ms,
            last_progress,
            "audio rendition read gave up — client will see 404"
        );
        Err(HlsCacheError::AudioNotReady {
            name: name.to_string(),
            reason: give_up,
            waited_ms,
            last_progress,
        })
    }

    async fn touch(&self, key: SegmentIdentity) {
        let mut state = self.state.lock().await;
        state.access_counter += 1;
        let counter = state.access_counter;
        if let Some(meta) = state.entries.get_mut(&key) {
            meta.last_used = counter;
        }
    }

    async fn record(&self, key: SegmentIdentity, bytes: u64) {
        let mut state = self.state.lock().await;
        state.access_counter += 1;
        let counter = state.access_counter;
        // If a previous entry existed under this key (rare — only on
        // disk-bypass tests), subtract its bytes first.
        if let Some(old) = state.entries.insert(
            key,
            EntryMeta {
                bytes,
                last_used: counter,
            },
        ) {
            state.total_bytes = state.total_bytes.saturating_sub(old.bytes);
        }
        state.total_bytes = state.total_bytes.saturating_add(bytes);
    }

    async fn maybe_evict(&self) {
        // Snapshot the (key, last_used) candidates outside the lock so
        // the disk delete doesn't hold the cache state.
        let mut to_remove: Vec<(SegmentIdentity, PathBuf)> = Vec::new();
        {
            let mut state = self.state.lock().await;
            while state.total_bytes > self.max_bytes {
                let Some((key, meta)) =
                    state
                        .entries
                        .iter()
                        .min_by_key(|(_, m)| m.last_used)
                        .map(|(k, m)| {
                            (
                                *k,
                                EntryMeta {
                                    bytes: m.bytes,
                                    last_used: m.last_used,
                                },
                            )
                        })
                else {
                    break;
                };
                state.entries.remove(&key);
                state.total_bytes = state.total_bytes.saturating_sub(meta.bytes);
                to_remove.push((key, self.segment_path_keyed(key)));
            }
        }
        for (_, path) in to_remove {
            let _ = tokio::fs::remove_file(&path).await;
        }
    }

    #[cfg(test)]
    async fn total_bytes(&self) -> u64 {
        self.state.lock().await.total_bytes
    }

    #[cfg(test)]
    async fn entry_count(&self) -> usize {
        self.state.lock().await.entries.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_transcode::{AudioDelivery, ContinuousAudio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    /// A storage failure and an encode failure must not share a label — they
    /// call for opposite responses (check the mount vs read the ffmpeg error),
    /// and `outcome="failed"` alone cannot tell them apart.
    #[test]
    fn a_missing_source_is_labelled_apart_from_an_encode_failure() {
        let missing = HlsCacheError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ));
        let denied = HlsCacheError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        let encode = HlsCacheError::Transcode("ffmpeg exploded".into());
        assert_eq!(failure_reason(&missing), "source_missing");
        assert_eq!(failure_reason(&denied), "permission");
        assert_eq!(failure_reason(&encode), "transcode");
        assert_ne!(failure_reason(&missing), failure_reason(&encode));
    }

    /// A 6.006 s video segment's options, matching the production grid.
    fn segment_transcode_opts() -> TranscodeOptions {
        SegmentOpts {
            container: SegmentContainer::Mpegts,
            video: Some(SegmentVideo::H264),
            audio: AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Aac,

                bitrate_bps: Some(128_000),
            }),

            video_bitrate_bps: Some(2_000_000),
            window: pharos_core::SegmentWindow::for_segment(
                27,
                pharos_core::FrameRate::from_mille(23_976),
                Some(3_600.0),
            ),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        }
        .resolve_with(|_| {
            Ok::<_, ()>(pharos_transcode::options::MuxedAudio {
                path: std::path::PathBuf::from("/cache/continuous.m4a"),
                start_seconds: 0.0,
            })
        })
        .expect("slice supplied")
        .to_transcode_options()
    }

    /// The same shape as [`segment_transcode_opts`], stopping at the
    /// UNRESOLVED `SegmentOpts` the cache is keyed on.
    fn segment_opts() -> SegmentOpts {
        SegmentOpts {
            container: SegmentContainer::Mpegts,
            video: Some(SegmentVideo::H264),
            audio: AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Aac,
                bitrate_bps: Some(128_000),
            }),
            video_bitrate_bps: Some(2_000_000),
            window: pharos_core::SegmentWindow::for_segment(
                27,
                pharos_core::FrameRate::from_mille(23_976),
                Some(3_600.0),
            ),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        }
    }

    /// V91 — a segment served from cache must be as countable as one that was
    /// transcoded. The miss path recorded twelve fields; the hit path recorded
    /// a bare counter on ONE of its two branches and nothing on the other, so
    /// "how many of these requests were hits?" had no answer — which is where
    /// the 2026-07-28 playback investigation stalled, holding 647 segment
    /// requests it could not split.
    ///
    /// Asserts the labels too: they are the dashboard contract, and a hit that
    /// lands under the wrong `class` or `hit_path` is worse than no signal
    /// because it reads as a healthy number.
    #[test]
    fn a_cache_hit_is_counted_and_labelled() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1 << 30);
        let opts = segment_opts();
        let key = SegmentIdentity::new(7, 27, None, None, &opts);
        let path = cache.segment_path_keyed(key);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // The source path deliberately does not exist: a hit must return
        // WITHOUT reaching the transcoder, so this test cannot pass by
        // accidentally encoding something.
        let bytes = metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                tokio::fs::create_dir_all(path.parent().unwrap())
                    .await
                    .unwrap();
                tokio::fs::write(&path, b"cached-segment-bytes")
                    .await
                    .unwrap();
                cache
                    .segment_bytes_keyed(
                        7,
                        27,
                        None,
                        None,
                        Path::new("/definitely/not/a/real/source.mkv"),
                        &opts,
                        JobClass::Interactive,
                    )
                    .await
                    .unwrap()
            })
        });

        assert_eq!(
            bytes, b"cached-segment-bytes",
            "a hit must return the CACHED bytes"
        );

        let hit = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_segment_cache_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                Some((labels, v))
            });

        let (labels, value) =
            hit.expect("a cache hit must emit pharos_segment_cache_total — it is the signal");
        for want in ["result=hit", "hit_path=fast", "class=interactive"] {
            assert!(
                labels.contains(&want.to_string()),
                "missing label {want}; got {labels:?}"
            );
        }
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one hit, got {value:?}"
        );
    }

    /// Bytes the stub encoder emits. Comfortably above `MIN_SEGMENT_BYTES`
    /// (64), so the produced "segment" is not rejected as truncated.
    const SLOW_SEGMENT_BYTES: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A cache whose encoder is a shell script that takes `delay` to produce a
    /// segment, and appends one byte to a counter file every time it RUNS.
    ///
    /// Returns the cache and the counter path: `encode_count` reads it, so the
    /// assertion "this segment was encoded once" is made against real process
    /// starts rather than against anything the cache reports about itself.
    ///
    /// The cache root is a SUBDIRECTORY of `dir`: `HlsSegmentCache::new`
    /// reconciles the generation marker by wiping everything else in its root,
    /// which would delete the stub and the counter.
    fn slow_test_cache(dir: &TempDir, delay: std::time::Duration) -> (HlsSegmentCache, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.path().join("slow-ffmpeg");
        let encodes = dir.path().join("encodes");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf 'e' >> '{}'\nsleep {}\nprintf '%s' '{}'\n",
                encodes.display(),
                delay.as_secs_f64(),
                SLOW_SEGMENT_BYTES,
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cache = HlsSegmentCache::new(dir.path().join("cache"), 1 << 30).with_ffmpeg(&bin);
        (cache, encodes)
    }

    /// How many times the stub encoder actually ran.
    fn encode_count(encodes: &Path) -> u64 {
        std::fs::metadata(encodes).map(|m| m.len()).unwrap_or(0)
    }

    /// Options the stub can satisfy: no video judgement, no continuous-audio
    /// slice to resolve (`Separate` never calls the resolver).
    fn slow_opts() -> SegmentOpts {
        SegmentOpts {
            container: SegmentContainer::Mpegts,
            video: None,
            audio: AudioDelivery::Separate,
            video_bitrate_bps: None,
            window: pharos_core::SegmentWindow::for_segment(0, None, Some(600.0)),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        }
    }

    /// The lock this replaces was held for the ENTIRE multi-second transcode.
    /// It was not guarding a data race — it was single-flight dedup implemented
    /// as mutual exclusion, and holding exclusion across seconds of work is what
    /// makes a queued speculative job poisonous (B108). A second requester must
    /// await the RESULT, and get it at the moment the first one does.
    #[tokio::test]
    async fn a_second_requester_awaits_the_result_rather_than_the_lock() {
        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(400));
        let opts = slow_opts();

        let started = std::time::Instant::now();
        let a = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(1, 0, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };
        // Arrive well after the first requester has begun the encode.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let b = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(1, 0, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };

        let a = a.await.unwrap().unwrap();
        let b = b.await.unwrap().unwrap();
        assert_eq!(a, b, "both requesters get the same bytes");
        assert_eq!(a, SLOW_SEGMENT_BYTES.as_bytes(), "the stub's payload");
        assert_eq!(
            encode_count(&encodes),
            1,
            "the second requester started a second encode"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(700),
            "the second requester serialised behind a second encode: {:?}",
            started.elapsed()
        );
    }

    /// Cancellation safety, which the Mutex design cannot express: today, if the
    /// lock holder is dropped mid-transcode the guard releases, the next waiter
    /// re-checks the filesystem, finds nothing, and encodes the SAME segment
    /// again. A detached driver outlives the requester that started it.
    #[tokio::test]
    async fn cancelling_the_first_requester_does_not_abort_the_encode() {
        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(400));
        let opts = slow_opts();

        let a = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(1, 0, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let b = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(1, 0, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };
        // Give the second requester the chance to attach to the in-flight
        // encode before the first one goes away, which is the shape the live
        // path has: a client seeks away while another request is already
        // waiting on the same segment.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // The client seeked away / disconnected.
        a.abort();

        let bytes = b.await.unwrap().unwrap();
        assert!(!bytes.is_empty(), "the surviving requester got nothing");
        assert_eq!(
            encode_count(&encodes),
            1,
            "the segment was encoded twice after a cancellation"
        );
    }

    /// The two `watch` properties the wait loop rests on, asserted rather than
    /// assumed — both are silent, concurrency-only failures if they do not hold.
    ///
    /// 1. A sender dropped WITHOUT publishing must wake its waiters with an
    ///    error. Otherwise a driver that panics parks every requester for that
    ///    segment forever, and the hang appears only under load.
    /// 2. A value published BEFORE the sender drops must still be delivered.
    ///    That is what makes the driver's publish-then-deregister order safe: a
    ///    requester that cloned the receiver a moment earlier still sees the
    ///    result, rather than losing the wakeup to the sender's disappearance.
    #[tokio::test]
    async fn a_dropped_driver_wakes_its_waiters_and_a_published_result_survives_it() {
        let (tx, mut rx) = tokio::sync::watch::channel(None::<u32>);
        drop(tx);
        assert!(
            rx.changed().await.is_err(),
            "a sender that died without publishing must wake its waiter"
        );

        let (tx, rx) = tokio::sync::watch::channel(None::<u32>);
        // Cloned while the encode is still in flight, as a coalescing
        // requester does.
        let mut waiter = rx.clone();
        tx.send(Some(7)).unwrap();
        drop(tx);
        assert!(
            waiter.changed().await.is_ok(),
            "a result published before the sender dropped must still be delivered"
        );
        assert_eq!(waiter.borrow_and_update().clone(), Some(7));
    }

    /// V91 symmetry for the path that replaces `post_lock`: a request that
    /// coalesced onto somebody else's in-flight encode is a HIT — it paid a wait
    /// but no work — and must be as countable as the fast one. The old post-lock
    /// re-check recorded nothing at all, which is the hole B134 was.
    #[test]
    fn a_coalesced_hit_is_counted_and_labelled() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, _encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(400));
        let opts = slow_opts();

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        // `with_local_recorder` installs the recorder for a CLOSURE on this
        // thread, so the runtime has to be current-thread: the detached driver
        // and both requesters then record into the same snapshot.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let a = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(9, 3, Path::new("/no/source"), &o, JobClass::Interactive)
                            .await
                    })
                };
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let b = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(9, 3, Path::new("/no/source"), &o, JobClass::Interactive)
                            .await
                    })
                };
                a.await.unwrap().unwrap();
                b.await.unwrap().unwrap();
            })
        });

        let hit = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_segment_cache_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                if !labels.contains(&"hit_path=coalesced".to_string()) {
                    return None;
                }
                Some((labels, v))
            });

        let (labels, value) = hit.expect(
            "a request served by somebody else's in-flight encode must emit \
             pharos_segment_cache_total{hit_path=coalesced} — it is the signal",
        );
        for want in ["result=hit", "hit_path=coalesced", "class=interactive"] {
            assert!(
                labels.contains(&want.to_string()),
                "missing label {want}; got {labels:?}"
            );
        }
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one coalesced hit, got {value:?}"
        );
    }

    /// Register an in-flight encode the TEST drives, so its outcome and its
    /// timing are chosen rather than raced for. Nothing production-only: this is
    /// the same registration `register_or_join` makes, and the requester under
    /// test reaches it through the ordinary public entry point.
    fn seed_inflight(
        cache: &HlsSegmentCache,
        key: SegmentIdentity,
    ) -> tokio::sync::watch::Sender<Option<SharedSegment>> {
        let (tx, rx) = tokio::sync::watch::channel(None);
        cache.inflight.insert(key, InFlightSegment { rx });
        tx
    }

    /// End the seeded encode exactly as a real driver ends: publish the outcome,
    /// THEN drop the registration, THEN drop the sender. That order is the one
    /// the `InFlightGuard` enforces, and a test that used any other order would
    /// be testing a driver pharos does not have.
    fn finish_seeded_driver(
        cache: &HlsSegmentCache,
        key: SegmentIdentity,
        tx: tokio::sync::watch::Sender<Option<SharedSegment>>,
        outcome: SharedSegment,
    ) {
        let _ = tx.send(Some(outcome));
        cache.inflight.remove(&key);
        drop(tx);
    }

    /// `SchedulerBusy` is LOAD-SHEDDING, not breakage — the scheduler keeping
    /// its permits for work a client is blocked on. It is the right answer for
    /// the speculative prefetch that asked for it and the wrong one for a client
    /// waiting on the segment, which sees it as a 500 on a video segment.
    ///
    /// Prefetch, cold-start prewarm and burn warm-up all submit as `Background`,
    /// and Background is shed precisely when the devices are busy — so without
    /// this, shed probability approaches 1 exactly when an interactive request
    /// most needs to be admitted. Under the mutex this design replaced, the
    /// waiter re-checked the filesystem after the shed and submitted its own job
    /// at its own class; a requester must not lose that.
    #[tokio::test]
    async fn a_coalescing_requester_re_drives_rather_than_inheriting_a_shed() {
        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(50));
        let opts = slow_opts();
        let key = SegmentIdentity::new(4, 2, None, None, &opts);
        let tx = seed_inflight(&cache, key);

        let waiter = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(4, 2, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };
        // Let it coalesce onto the in-flight encode before that encode is shed.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            encode_count(&encodes),
            0,
            "the requester started its own encode instead of coalescing"
        );

        finish_seeded_driver(&cache, key, tx, Err(Arc::new(HlsCacheError::SchedulerBusy)));

        let bytes = waiter
            .await
            .unwrap()
            .expect("a client request was failed by another job's load-shed decision");
        assert_eq!(bytes, SLOW_SEGMENT_BYTES.as_bytes());
        assert_eq!(
            encode_count(&encodes),
            1,
            "the requester did not drive an encode of its own"
        );
    }

    /// A scheduler that sheds every speculative job: one device, whose whole
    /// capacity is the headroom reserved for client work, so a `Background`
    /// submission is refused before it is ever dispatched. The spawner is
    /// therefore never called — a shed costs no worker.
    fn always_sheds_background() -> pharos_transcode::scheduler::TranscodeScheduler {
        use pharos_transcode::scheduler::{SchedConfig, SpawnFuture, TranscodeScheduler};

        struct NeverSpawns;
        impl pharos_transcode::scheduler::WorkerSpawner for NeverSpawns {
            fn spawn(&self, _id: pharos_transcode::protocol::WorkerId) -> SpawnFuture {
                Box::pin(async {
                    Err(std::io::Error::other(
                        "the shed path must not need a worker",
                    ))
                })
            }
        }

        TranscodeScheduler::spawn(
            pharos_transcode::device::DeviceTable::from_probe(&[], 1),
            Arc::new(NeverSpawns),
            SchedConfig {
                background_headroom: 1,
                ..SchedConfig::default()
            },
        )
    }

    /// A DRIVING requester keeps its own shed. Shedding work you submitted
    /// yourself is the intended behaviour (B108/V58), and the fall-through above
    /// must not quietly become a retry loop that defeats it: the re-drive is
    /// "do not adopt somebody else's decision", not "ask again until admitted".
    #[tokio::test]
    async fn a_driving_requester_keeps_its_own_shed() {
        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let cache = cache.with_scheduler(always_sheds_background());
        let opts = slow_opts();

        // Nothing is in flight, so this request drives its own encode — and its
        // own encode is shed.
        let err = cache
            .segment_bytes(5, 1, Path::new("/no/source"), &opts, JobClass::Background)
            .await
            .expect_err("a speculative job the scheduler declined must report the shed");
        assert!(
            matches!(err, HlsCacheError::SchedulerBusy),
            "a driving requester must surface its own shed unchanged: {err:?}"
        );
        assert_eq!(
            encode_count(&encodes),
            0,
            "a shed job must not fall back to encoding anyway"
        );
        assert!(
            !cache
                .inflight
                .contains_key(&SegmentIdentity::new(5, 1, None, None, &opts)),
            "the shed left its registration behind"
        );
    }

    /// A driver task dropped before its FIRST poll must still deregister.
    ///
    /// Runtime shutdown does exactly this, and a guard constructed INSIDE the
    /// spawned block is never constructed on that path — while the block's
    /// captured sender drops anyway. The registration then outlives the only
    /// channel that could ever publish to it, and every later request for that
    /// key coalesces onto it and gets the dead-driver error forever: a permanent
    /// per-segment outage produced by an orderly shutdown.
    ///
    /// Deterministic, not raced: on a current-thread runtime the spawned task
    /// cannot run until the spawning future yields, and this one never yields.
    #[test]
    fn a_driver_dropped_before_its_first_poll_leaves_no_registration() {
        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let opts = slow_opts();
        let key = SegmentIdentity::new(3, 0, None, None, &opts);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_rx, driving) = cache.register_or_join(
                key,
                Path::new("/no/source"),
                &opts,
                3,
                0,
                JobClass::Interactive,
            );
            assert!(driving, "nothing was in flight, so this call must drive");
            assert!(
                cache.inflight.contains_key(&key),
                "the driver never registered"
            );
        });
        // Shutdown drops the queued task without ever polling it.
        drop(rt);

        assert_eq!(
            encode_count(&encodes),
            0,
            "the driver ran; this test is no longer about an unpolled task"
        );
        assert!(
            !cache.inflight.contains_key(&key),
            "a never-polled driver left its registration behind — every later request \
             for this segment would now wait on a channel nobody can publish to"
        );
    }

    /// Write an ffmpeg `-progress` sidecar for `out`. Shaped like the real
    /// thing: repeated blocks, only the last of which is final.
    async fn write_progress(out: &Path, frames: u64, out_time_secs: f64) {
        let us = (out_time_secs * 1e6) as u64;
        let body = format!(
            "frame=1\nfps=0.00\nout_time_us=41708\nprogress=continue\n\
             frame={frames}\nfps=120.0\nout_time_us={us}\nprogress=end\n"
        );
        tokio::fs::write(progress_sidecar_path(out), body)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_complete_segment_is_accepted() {
        let td = TempDir::new().unwrap();
        let out = td.path().join("seg.ts");
        write_progress(&out, 144, 6.006).await;
        let got = read_progress(&out).await;
        assert_eq!(short_of_frames(got, &segment_transcode_opts()), None);
    }

    #[tokio::test]
    async fn a_segment_with_no_video_frames_is_rejected() {
        // The observed catastrophic case: the decoder could not rebuild its
        // reference list after the seek and skipped every NALU, so the
        // segment carries audio and no picture — and ffmpeg exited 0. Two
        // live segments of Fringe S01E02 looked exactly like this.
        let td = TempDir::new().unwrap();
        let out = td.path().join("seg.ts");
        write_progress(&out, 0, 6.006).await;
        let got = read_progress(&out).await;
        let why = short_of_frames(got, &segment_transcode_opts())
            .expect("a video segment with zero frames must be rejected");
        assert!(why.contains("no video frames"), "{why}");
    }

    #[tokio::test]
    async fn an_audio_only_segment_is_not_judged_on_video_frames() {
        // The `/hls1/{A64..A256}` ladder legitimately produces no video.
        let td = TempDir::new().unwrap();
        let out = td.path().join("seg.ts");
        write_progress(&out, 0, 6.006).await;
        let mut opts = segment_transcode_opts();
        opts.video = None;
        opts.video_bitrate_bps = None;
        let got = read_progress(&out).await;
        assert_eq!(short_of_frames(got, &opts), None);
    }

    #[tokio::test]
    async fn a_truncated_segment_is_rejected() {
        let td = TempDir::new().unwrap();
        let out = td.path().join("seg.ts");
        write_progress(&out, 90, 3.5).await;
        let got = read_progress(&out).await;
        let why = short_of_frames(got, &segment_transcode_opts())
            .expect("a segment that stopped early must be rejected");
        assert!(why.contains("3.500") && why.contains("6.006"), "{why}");
    }

    #[tokio::test]
    async fn a_missing_progress_report_accepts_rather_than_rejects() {
        // A missing report is not evidence of a bad segment. Failing closed
        // here would reject every segment from any path that does not write
        // one — an outage, in exchange for no information.
        let td = TempDir::new().unwrap();
        let out = td.path().join("seg.ts");
        let got = read_progress(&out).await;
        assert_eq!(short_of_frames(got, &segment_transcode_opts()), None);
    }

    #[tokio::test]
    async fn the_progress_sidecar_does_not_outlive_the_check() {
        // It sits in the cache directory next to the segment; left behind it
        // would accumulate one file per segment ever produced, invisible to
        // the LRU accounting.
        let td = TempDir::new().unwrap();
        let out = td.path().join("seg.ts");
        write_progress(&out, 144, 6.006).await;
        // The sidecar is consumed by the READ now, not by the check — the read
        // moved out so its numbers can be reported as well as judged.
        let _ = read_progress(&out).await;
        assert!(
            !tokio::fs::try_exists(progress_sidecar_path(&out))
                .await
                .unwrap(),
            "sidecar left behind"
        );
    }

    /// The Google TV outage: `hls1/*.ts` segments shipped with NO audio stream
    /// (`nb_streams: 1`, video only) under a playlist advertising audio, so
    /// ExoPlayer fetched one segment and stopped. Confirmed on the real bytes
    /// pulled back from prod.
    ///
    /// The cause was a guard reading `to_transcode_options().audio`, which is
    /// `None` UNCONDITIONALLY ("a segment never runs an audio encoder"), so it
    /// could never fire and the argv fell through to `-an`. That guard is now
    /// gone: `SegmentOpts::resolve` runs its closure for exactly the deliveries
    /// that need a slice and moves the result in, so the broken state has no
    /// representation. This test pins the two halves of that.
    #[tokio::test]
    async fn only_a_muxed_segment_asks_for_a_continuous_audio_slice() {
        let slice = pharos_transcode::options::MuxedAudio {
            path: std::path::PathBuf::from("/tmp/continuous.m4a"),
            start_seconds: 0.0,
        };

        // Muxed: the closure IS called, and its result reaches the argv as a
        // copy source — never `-an`.
        let muxed = ident_opts(
            Some(SegmentVideo::H264),
            Some(SegmentAudio::Aac),
            SegmentContainer::Mpegts,
            Some(4_000_000),
            Some(128_000),
        );
        let mut asked = false;
        let resolved = muxed
            .resolve(|c| {
                asked = true;
                assert_eq!(c.codec, SegmentAudio::Aac);
                std::future::ready(Ok::<_, HlsCacheError>(slice.clone()))
            })
            .await
            .unwrap();
        assert!(asked, "a muxed segment must ask for its slice");
        assert!(resolved.to_transcode_options().muxed_audio_source.is_some());

        // Separate: a video-only rung must NOT spawn a continuous encode it
        // will never mux.
        let separate = ident_opts(
            Some(SegmentVideo::Vp9),
            None,
            SegmentContainer::Fmp4,
            Some(4_000_000),
            None,
        );
        let mut asked = false;
        let resolved = separate
            .resolve(|_| {
                asked = true;
                std::future::ready(Ok::<_, HlsCacheError>(slice.clone()))
            })
            .await
            .unwrap();
        assert!(!asked, "an audio-free segment must not request a slice");
        assert!(resolved.to_transcode_options().muxed_audio_source.is_none());
    }

    #[test]
    fn a_segment_missing_frames_is_measurable_even_though_the_gross_check_passes() {
        // The gap this instrumentation exists to close. `short_of_frames` only
        // rejects a segment that missed 10% of its duration; a 6.006 s window
        // that produced 5.9 s and 141 of its 144 frames sails through — and
        // three missing frames at a boundary is a visible hitch, cached, so it
        // replays on every later view.
        let opts = segment_transcode_opts();
        assert_eq!(
            short_of_frames(Some((141, 5.9)), &opts),
            None,
            "the gross duration check cannot see a three-frame shortfall"
        );

        // The window CAN see it, because it kept the source frame rate.
        let rate = pharos_core::FrameRate::from_mille(23_976);
        let w = pharos_core::SegmentWindow::for_segment(1, rate, Some(1372.121));
        let want = w.expected_frames().expect("rate known");
        assert_eq!(want, 144);
        assert_eq!(want as i64 - 141i64, 3, "deficit is measurable in frames");

        // And an unprobed source yields no expectation at all rather than a
        // guessed one, so it cannot manufacture a phantom deficit.
        let unknown = pharos_core::SegmentWindow::for_segment(1, None, Some(1372.121));
        assert_eq!(unknown.expected_frames(), None);
    }

    /// Build a `SegmentOpts` carrying just the identity-relevant fields.
    fn ident_opts(
        video: Option<SegmentVideo>,
        audio: Option<SegmentAudio>,
        container: SegmentContainer,
        video_bitrate_bps: Option<u64>,
        audio_bitrate_bps: Option<u64>,
    ) -> SegmentOpts {
        SegmentOpts {
            container,
            video,
            audio: match audio {
                Some(codec) => AudioDelivery::Muxed(ContinuousAudio {
                    codec,
                    bitrate_bps: audio_bitrate_bps,
                }),
                None => AudioDelivery::Separate,
            },
            video_bitrate_bps,
            window: pharos_core::SegmentWindow::for_segment(0, None, Some(600.0)),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        }
    }

    #[test]
    fn segment_outcome_labels_are_stable_and_distinct() {
        // Dashboards and alerts key on these strings, so a rename breaks them
        // silently. Exhaustive match: a new outcome fails to compile here
        // until it is given a label and added below.
        let all = [
            SegmentOutcome::Ok,
            SegmentOutcome::Short,
            SegmentOutcome::Empty,
            SegmentOutcome::Failed,
            SegmentOutcome::Shed,
        ];
        for o in all {
            match o {
                SegmentOutcome::Ok
                | SegmentOutcome::Short
                | SegmentOutcome::Empty
                | SegmentOutcome::Failed
                | SegmentOutcome::Shed => {}
            }
        }
        let labels: Vec<&str> = all.iter().map(|o| o.label()).collect();
        assert_eq!(labels, vec!["ok", "short", "empty", "failed", "shed"]);
        let mut uniq = labels.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), labels.len(), "labels must be distinct");
    }

    #[test]
    fn recording_an_outcome_works_without_a_recorder_installed() {
        // The metrics facade is a no-op until the server installs the
        // Prometheus recorder, and pharos-cache is used in tests and by the
        // CLI where it never is. Emitting must not panic there.
        for o in [
            SegmentOutcome::Ok,
            SegmentOutcome::Short,
            SegmentOutcome::Empty,
            SegmentOutcome::Failed,
        ] {
            record_segment_outcome(o, JobClass::Interactive);
        }
    }

    #[test]
    fn every_identity_input_changes_both_the_cache_path_and_the_etag() {
        // The ETag and the cache path must move together. They used to be
        // derived separately — the ETag restated a hand-picked subset of the
        // key's inputs — and drifted, so a 304 could hand a client the other
        // variant's bytes.
        let base_opts = ident_opts(
            Some(SegmentVideo::H264),
            Some(SegmentAudio::Aac),
            SegmentContainer::Mpegts,
            Some(4_000_000),
            Some(128_000),
        );
        let base = SegmentIdentity::new(1, 5, Some(1), Some(2), &base_opts);

        let mut vbr = base_opts.clone();
        vbr.video_bitrate_bps = Some(2_000_000);
        let mut abr = base_opts.clone();
        abr.audio = AudioDelivery::Muxed(ContinuousAudio {
            codec: SegmentAudio::Aac,
            bitrate_bps: Some(256_000),
        });
        let mut codec = base_opts.clone();
        codec.container = SegmentContainer::Fmp4;

        let cases: Vec<(&str, SegmentIdentity)> = vec![
            (
                "media",
                SegmentIdentity::new(2, 5, Some(1), Some(2), &base_opts),
            ),
            (
                "segment index",
                SegmentIdentity::new(1, 6, Some(1), Some(2), &base_opts),
            ),
            (
                "audio track",
                SegmentIdentity::new(1, 5, Some(0), Some(2), &base_opts),
            ),
            (
                "subtitle burn",
                SegmentIdentity::new(1, 5, Some(1), None, &base_opts),
            ),
            (
                "video bitrate",
                SegmentIdentity::new(1, 5, Some(1), Some(2), &vbr),
            ),
            // Previously invisible: one "governing" bitrate took the video
            // figure whenever there was one, so two clients on the same video
            // rung with different audio bitrates shared a cache entry.
            (
                "audio bitrate",
                SegmentIdentity::new(1, 5, Some(1), Some(2), &abr),
            ),
            (
                "container",
                SegmentIdentity::new(1, 5, Some(1), Some(2), &codec),
            ),
        ];
        for (what, other) in cases {
            assert_ne!(other, base, "{what} must change the identity");
            assert_ne!(
                other.cache_relative_path(),
                base.cache_relative_path(),
                "{what} must change the cache path"
            );
            assert_ne!(other.etag(), base.etag(), "{what} must change the ETag");
        }
    }

    #[test]
    fn a_continuous_audio_session_reaches_back_past_the_decode_preroll() {
        // The segment's ffmpeg seeks BOTH inputs to one preroll before the
        // segment start, because two inputs seeked to different positions are
        // re-based by different amounts and the audio would land offset from
        // the video by the difference. A session that only reached the segment
        // start would be seeked before its own content.
        let want = 162.5;
        let start = HlsSegmentCache::continuous_audio_session_start_secs(want);
        assert!(
            start <= want - pharos_transcode::DECODE_PREROLL_SECONDS,
            "session starts at {start}, not far enough back for a preroll \
             before {want}"
        );
        // Snapped to the audio grid, so repeated requests around one playhead
        // resolve to the same session instead of one encode per segment.
        assert_eq!(start % HlsSegmentCache::AUDIO_SEGMENT_SECONDS, 0.0);
    }

    #[test]
    fn an_early_continuous_audio_request_starts_at_the_file_head() {
        for want in [0.0, 6.006, 15.0] {
            assert_eq!(
                HlsSegmentCache::continuous_audio_session_start_secs(want),
                0.0,
                "want {want}"
            );
        }
    }

    #[test]
    fn a_continuous_audio_session_is_source_anchored() {
        // `-ss X` and `-output_ts_offset X` cancel exactly, so every sample
        // keeps its true source timestamp and a seek session's output is
        // interchangeable with the whole-file one. They must agree to the
        // digit or they no longer cancel.
        let args = HlsSegmentCache::continuous_audio_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/_contaudio/1-a0-b128/s150"),
            Some(1),
            Some(128_000),
            150.0,
        )
        .unwrap();
        let at = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args[i + 1].clone())
        };
        assert_eq!(at("-ss"), Some("150.000000".into()));
        assert_eq!(at("-output_ts_offset"), Some("150.000000".into()));
        assert_eq!(at("-c:a"), Some("aac".into()));
        assert_eq!(at("-map"), Some("0:a:1".into()));
        assert_eq!(at("-f"), Some("mpegts".into()));
        // Without these the muxer's 1.4 s initial cue delay shifts every
        // sample away from the source timestamp this encode exists to keep.
        assert_eq!(at("-muxdelay"), Some("0".into()));
        assert_eq!(at("-muxpreload"), Some("0".into()));
        assert!(args.iter().any(|a| a == "-vn"), "{args:?}");
        assert_eq!(
            at("-progress"),
            Some("/c/_contaudio/1-a0-b128/s150/audio.ts.progress".into())
        );
    }

    #[test]
    fn a_whole_file_continuous_audio_session_has_no_anchor_to_cancel() {
        let args = HlsSegmentCache::continuous_audio_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/_contaudio/1-a0-b128"),
            None,
            None,
            0.0,
        )
        .unwrap();
        assert!(!args.iter().any(|a| a == "-ss"), "{args:?}");
        assert!(!args.iter().any(|a| a == "-output_ts_offset"), "{args:?}");
        assert!(args.iter().any(|a| a == "0:a:0?"), "{args:?}");
    }

    #[tokio::test]
    async fn continuous_audio_coverage_is_the_session_start_plus_its_progress() {
        // `out_time` counts from the session's own first sample and does not
        // include `-output_ts_offset`, so a seek session that reports 28 s has
        // covered its start plus 28 — reading it as an absolute position would
        // make every seek session look 150 s behind and wait forever.
        let td = TempDir::new().unwrap();
        tokio::fs::write(
            td.path().join("audio.ts.progress"),
            "out_time_us=1000000\nprogress=continue\nout_time_us=28000000\nprogress=end\n",
        )
        .await
        .unwrap();
        assert_eq!(
            HlsSegmentCache::continuous_audio_covered_secs(td.path(), 150.0).await,
            Some(178.0)
        );
        // Nothing produced yet is not "covered from 0".
        let empty = TempDir::new().unwrap();
        assert_eq!(
            HlsSegmentCache::continuous_audio_covered_secs(empty.path(), 150.0).await,
            None
        );
    }

    /// Seed a cache file directly (no ffmpeg) and update LRU state to
    /// match. Used by unit tests so they don't need a real ffmpeg
    /// invocation per byte.
    async fn force_insert(cache: &HlsSegmentCache, media_id: u64, seg: u32, body: &[u8]) {
        let path = cache.segment_path(media_id, seg);
        if let Some(p) = path.parent() {
            tokio::fs::create_dir_all(p).await.unwrap();
        }
        tokio::fs::write(&path, body).await.unwrap();
        cache
            .record(
                SegmentIdentity {
                    media_id,
                    seg_index: seg,
                    audio_index: 0,
                    subtitle_index: NO_SUBTITLE,
                    video_bitrate_kbps: 0,
                    audio_bitrate_kbps: 0,
                    codec_tag: 0,
                },
                body.len() as u64,
            )
            .await;
        cache.maybe_evict().await;
    }

    #[test]
    fn h264_mpegts_and_fmp4_get_distinct_cache_keys() {
        // Regression (live prod break): muxed-mpegts H264 (`hls1/*.ts`) and
        // audio-free fMP4 H264 (`h264cmaf/*`) share the codec and the
        // (media, seg, audio, bitrate) tuple but have TOTALLY different bytes.
        // Keying on the codec alone made an h264cmaf request read a
        // previously-cached mpegts segment, feed those bytes to the mp4 parser,
        // and 500 ("truncated box at offset 0") in ~4 ms (a cache hit on the
        // wrong bytes). The container must be part of the key.
        let mpegts = codec_tag(
            Some(SegmentVideo::H264),
            Some(SegmentAudio::Aac),
            SegmentContainer::Mpegts,
        );
        let fmp4 = codec_tag(Some(SegmentVideo::H264), None, SegmentContainer::Fmp4);
        let vp9 = codec_tag(Some(SegmentVideo::Vp9), None, SegmentContainer::Fmp4);
        assert_ne!(mpegts, fmp4, "muxed h264 and fMP4 h264 must not collide");
        assert_ne!(fmp4, vp9);
        // 24, not 8: B116 cached video-only muxed segments under tag 8, and a
        // cached poisoned segment outlives the fix that stopped producing it.
        // Verified against prod — refetching the exact segment after the fix
        // deployed still probed `nb_streams: 1` until this moved.
        // Bumped again past the dropped-frame generation: every hardware
        // encode under the previous tags is one frame short per segment.
        assert_eq!(
            mpegts, 28,
            "muxed-h264 tag bumped past the shifted-audio gen"
        );
        assert_eq!(vp9, 27, "vp9 tag bumped past the short-frame gen");

        // The on-disk keys differ for the same (media, seg, audio, bitrate).
        let key_ts = SegmentIdentity::new(
            1,
            0,
            Some(1),
            None,
            &ident_opts(
                Some(SegmentVideo::H264),
                Some(SegmentAudio::Aac),
                SegmentContainer::Mpegts,
                Some(4_000_000),
                None,
            ),
        );
        let key_m4 = SegmentIdentity::new(
            1,
            0,
            Some(1),
            None,
            &ident_opts(
                Some(SegmentVideo::H264),
                None,
                SegmentContainer::Fmp4,
                Some(4_000_000),
                None,
            ),
        );
        assert_ne!(key_ts, key_m4, "distinct cache keys per container");
    }

    #[test]
    fn audio_ladder_rungs_do_not_share_one_cache_entry() {
        // An audio-only item advertises a whole bitrate ladder
        // (/hls1/{A64,A96,A128,A192,A256}) as separate EXT-X-STREAM-INFs, and
        // the audio-variant branch clears `video`/`video_bitrate_bps`. Keying
        // the bitrate off the VIDEO bitrate alone therefore gave every rung the
        // same key (bitrate 0, codec tag 0): the first rung to transcode was
        // served for all of them, so ABR silently did nothing and a 64 kbps
        // client got 256 kbps bytes.
        let rung = |bps: u64| {
            SegmentIdentity::new(
                1,
                0,
                None,
                None,
                &ident_opts(
                    None,
                    Some(SegmentAudio::Aac),
                    SegmentContainer::Mpegts,
                    None,
                    Some(bps),
                ),
            )
        };
        let a64 = rung(64_000);
        let a256 = rung(256_000);
        assert_ne!(a64, a256, "audio rungs must key on their own bitrate");
        assert_eq!(a64.audio_bitrate_kbps, 64);
        assert_eq!(a256.audio_bitrate_kbps, 256);

        // The audio CODEC + container must separate audio-only segments too —
        // they all collapsed onto tag 0 while the video tag carried everything.
        let aac_ts = codec_tag(None, Some(SegmentAudio::Aac), SegmentContainer::Mpegts);
        let aac_m4 = codec_tag(None, Some(SegmentAudio::Aac), SegmentContainer::Fmp4);
        let opus_ts = codec_tag(None, Some(SegmentAudio::Opus), SegmentContainer::Mpegts);
        assert_ne!(aac_ts, aac_m4);
        assert_ne!(aac_ts, opus_ts);
        assert_ne!(aac_ts, 0, "an audio-only segment is not the 'no codec' tag");

        // A video segment is unaffected: its video bitrate still governs, so
        // every warm on-disk entry keeps its filename.
        let v = SegmentIdentity::new(
            1,
            0,
            Some(1),
            None,
            &ident_opts(
                Some(SegmentVideo::H264),
                Some(SegmentAudio::Aac),
                SegmentContainer::Mpegts,
                Some(4_000_000),
                Some(128_000),
            ),
        );
        assert_eq!(v.video_bitrate_kbps, 4_000);
        assert_eq!(v.audio_bitrate_kbps, 128);
    }

    #[tokio::test]
    async fn hit_returns_cached_bytes_without_calling_ffmpeg() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        force_insert(&cache, 7, 0, b"segment-bytes").await;
        let opts = SegmentOpts {
            container: pharos_transcode::SegmentContainer::Mpegts,
            video: None,
            audio: AudioDelivery::Separate,
            video_bitrate_bps: None,
            window: pharos_core::SegmentWindow::for_segment(0, None, Some(600.0)),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        };
        let got = cache
            .segment_bytes(7, 0, Path::new("/no/source"), &opts, JobClass::Interactive)
            .await
            .unwrap();
        assert_eq!(got, b"segment-bytes");
    }

    #[tokio::test]
    async fn miss_with_unavailable_ffmpeg_propagates_error() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        let opts = SegmentOpts {
            container: pharos_transcode::SegmentContainer::Mpegts,
            video: None,
            audio: AudioDelivery::Separate,
            video_bitrate_bps: None,
            window: pharos_core::SegmentWindow::for_segment(0, None, Some(600.0)),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        };
        let res = cache
            .segment_bytes(8, 0, Path::new("/no/source"), &opts, JobClass::Interactive)
            .await;
        assert!(matches!(res, Err(HlsCacheError::Transcode(_))));
    }

    #[tokio::test]
    async fn lru_eviction_drops_least_recent_when_over_cap() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 20);
        // 3 segments of 10 bytes each — total 30, cap 20 -> 1 must go.
        force_insert(&cache, 7, 0, b"0123456789").await;
        force_insert(&cache, 7, 1, b"0123456789").await;
        // Touch seg 0 so it's more-recent than seg 1.
        let opts = SegmentOpts {
            container: pharos_transcode::SegmentContainer::Mpegts,
            video: None,
            audio: AudioDelivery::Separate,
            video_bitrate_bps: None,
            window: pharos_core::SegmentWindow::for_segment(0, None, Some(600.0)),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        };
        let _ = cache
            .segment_bytes(7, 0, Path::new("/no/source"), &opts, JobClass::Interactive)
            .await
            .unwrap();
        // Adding seg 2 should evict seg 1 (the LRU).
        force_insert(&cache, 7, 2, b"0123456789").await;
        assert!(cache.total_bytes().await <= 20);
        assert_eq!(cache.entry_count().await, 2);
        // seg 1 must be gone from disk too.
        assert!(!tokio::fs::try_exists(td.path().join("7").join("1.ts"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn concurrent_hits_share_one_decode() {
        // Two concurrent requests for the same segment must both read
        // the cached file rather than racing two transcodes. Use a
        // stand-in transcoder that counts invocations to prove only
        // one fired.
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 4096);
        // Pre-seed so both calls hit the fast path.
        force_insert(&cache, 9, 0, b"abc").await;
        let counter = AtomicU32::new(0);
        let one = async {
            counter.fetch_add(1, Ordering::SeqCst);
            let opts = SegmentOpts {
                container: pharos_transcode::SegmentContainer::Mpegts,
                video: None,
                audio: AudioDelivery::Separate,
                video_bitrate_bps: None,
                window: pharos_core::SegmentWindow::for_segment(0, None, Some(600.0)),
                audio_source_stream_index: None,
                burn_subtitle_stream_index: None,
                burn_subtitle_is_text: false,
                burn_subtitle_ass_path: None,
                burn_fonts_dir: None,
            };
            cache
                .segment_bytes(9, 0, Path::new("/n"), &opts, JobClass::Interactive)
                .await
                .unwrap()
        };
        let (a, b) = tokio::join!(one, async {
            counter.fetch_add(1, Ordering::SeqCst);
            let opts = SegmentOpts {
                container: pharos_transcode::SegmentContainer::Mpegts,
                video: None,
                audio: AudioDelivery::Separate,
                video_bitrate_bps: None,
                window: pharos_core::SegmentWindow::for_segment(0, None, Some(600.0)),
                audio_source_stream_index: None,
                burn_subtitle_stream_index: None,
                burn_subtitle_is_text: false,
                burn_subtitle_ass_path: None,
                burn_fonts_dir: None,
            };
            cache
                .segment_bytes(9, 0, Path::new("/n"), &opts, JobClass::Interactive)
                .await
                .unwrap()
        });
        assert_eq!(a, b);
        assert_eq!(a, b"abc");
    }

    /// B42 — the from-0 audio session must stay byte-identical to the old
    /// behaviour: no seek, no renumbering, no timestamp offset, canonical
    /// playlist name (its presence is the done-marker).
    #[test]
    fn audio_hls_args_from_zero_has_no_seek_or_offset() {
        let a = HlsSegmentCache::audio_hls_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/d"),
            Some(1),
            Some(128_000),
            0,
            0.0,
        )
        .unwrap();
        let joined = a.join(" ");
        assert!(!joined.contains("-ss"), "{joined}");
        assert!(!joined.contains("-start_number"), "{joined}");
        assert!(!joined.contains("-output_ts_offset"), "{joined}");
        // The whole-file session owns the rendition root, so its playlist is
        // the root `audio.m3u8` that doubles as the done-marker.
        assert!(joined.ends_with("/c/d/audio.m3u8"), "{joined}");
        assert!(joined.contains("/c/d/a%d.m4s"), "{joined}");
        assert!(joined.contains("-map 0:a:1"), "{joined}");
    }

    /// B120 — a whole-file session must carry the audio track's own start time
    /// forward, or its fragments hold content from that much later than the
    /// grid position they are served at.
    #[test]
    fn a_from_zero_session_pads_a_late_starting_audio_track() {
        let a = HlsSegmentCache::audio_hls_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/d"),
            None,
            Some(128_000),
            0,
            1.7,
        )
        .unwrap();
        let joined = a.join(" ");
        assert!(
            joined.contains("-af adelay=delays=1700.000ms:all=1"),
            "{joined}"
        );
        // The pad belongs before the encoder, not after it.
        let af = a.iter().position(|x| x == "-af").unwrap();
        let ca = a.iter().position(|x| x == "-c:a").unwrap();
        assert!(af < ca, "{joined}");
    }

    /// A track that starts with its video needs no pad — the overwhelming
    /// majority of files, which must keep the argv they always had.
    #[test]
    fn a_punctual_audio_track_is_not_padded() {
        let a = HlsSegmentCache::audio_hls_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/d"),
            None,
            Some(128_000),
            0,
            0.0,
        )
        .unwrap();
        assert!(!a.join(" ").contains("adelay"), "{a:?}");
    }

    /// A SEEK session lands on the requested source position exactly, so
    /// padding it would push its content late by the offset instead.
    #[test]
    fn a_seek_session_is_never_padded() {
        let a = HlsSegmentCache::audio_hls_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/d"),
            None,
            Some(128_000),
            30,
            1.7,
        )
        .unwrap();
        assert!(!a.join(" ").contains("adelay"), "{a:?}");
    }

    /// B42 — a seek session must be source-anchored: input-seek to the
    /// segment boundary and absolute segment numbering, so its files line up
    /// with the whole-file session's. Its playlist must not clobber the from-0
    /// session's done-marker.
    ///
    /// `-output_ts_offset` does NOT reach the fragment timestamps — ffmpeg's
    /// HLS muxer numbers `tfdt` from the session's own first sample whatever it
    /// is passed (B121, measured on ffmpeg 8.1). The anchor is corrected when
    /// the fragment is served; the option stays because it costs nothing and
    /// keeps the seek and the offset cancelling for content selection.
    #[test]
    fn audio_hls_args_seek_session_is_source_anchored() {
        let a = HlsSegmentCache::audio_hls_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/d"),
            None,
            Some(128_000),
            30,
            0.0,
        )
        .unwrap();
        let joined = a.join(" ");
        assert!(joined.contains("-ss 180.000000"), "{joined}");
        assert!(joined.contains("-output_ts_offset 180.000000"), "{joined}");
        assert!(joined.contains("-start_number 30"), "{joined}");
        // A seek session writes into its OWN directory, so neither its
        // playlist nor its segments can clobber the whole-file session's.
        assert!(joined.ends_with("/c/d/s30/audio.m3u8"), "{joined}");
        assert!(joined.contains("/c/d/s30/a%d.m4s"), "{joined}");
        // -ss must be an INPUT option (before -i).
        let ss = a.iter().position(|x| x == "-ss").unwrap();
        let i = a.iter().position(|x| x == "-i").unwrap();
        assert!(ss < i, "-ss must precede -i: {joined}");
    }

    /// The seek anchor sits on the SAME uniform grid `-hls_time` cuts on, and
    /// the `-output_ts_offset` cancels the `-ss` exactly.
    ///
    /// This replaces an assertion that the anchor be frame-snapped to the video
    /// grid. That was wrong on its own terms: `-ss X` with a matching
    /// `-output_ts_offset X` cancel, so every sample keeps its true source
    /// timestamp and the anchor cannot produce A/V skew either way — it only
    /// decides which samples land in which FILE. Snapping it away from the
    /// uniform grid meant a seek session cut its segments up to half a video
    /// frame from where the whole-file session cut the same indices; measured on
    /// a real 23.976 source, `a5` began at 30.0065 s from one session and
    /// 29.9875 s from the other.
    #[test]
    fn audio_seek_anchor_matches_the_segmenter_grid_and_cancels_exactly() {
        for seg in [1u32, 5, 30, 1000] {
            let a = HlsSegmentCache::audio_hls_args(
                Path::new("/m/x.mkv"),
                Path::new("/c/d"),
                None,
                Some(128_000),
                seg,
                0.0,
            )
            .unwrap();
            let at = |flag: &str| {
                a.iter()
                    .position(|x| x == flag)
                    .map(|i| a[i + 1].clone())
                    .unwrap()
            };
            let want = format!("{:.6}", seg as f64 * 6.0);
            assert_eq!(at("-ss"), want, "seg {seg}");
            assert_eq!(at("-output_ts_offset"), want, "seg {seg}");
            // Same string on both, so they cancel to the exact source timestamp.
            assert_eq!(at("-ss"), at("-output_ts_offset"), "seg {seg}");
            assert_eq!(at("-hls_time"), "6", "seg {seg}");
        }
    }

    #[test]
    fn audio_sessions_never_share_a_segment_filename() {
        let root = Path::new("/c/d");
        assert_eq!(HlsSegmentCache::audio_session_dir(root, 0), root);
        assert_eq!(
            HlsSegmentCache::audio_session_dir(root, 30),
            root.join("s30")
        );
        assert_ne!(
            HlsSegmentCache::audio_session_dir(root, 0).join("a30.m4s"),
            HlsSegmentCache::audio_session_dir(root, 30).join("a30.m4s"),
        );
    }

    #[tokio::test]
    async fn audio_reads_resolve_to_the_deepest_session_that_has_the_segment() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        // Whole-file session has caught up to a40; a seek session started at
        // a30 also wrote a30..a40 with its own (different) cut points.
        tokio::fs::create_dir_all(root.join("s30")).await.unwrap();
        for n in [5u32, 30, 40] {
            tokio::fs::write(root.join(format!("a{n}.m4s")), b"from0")
                .await
                .unwrap();
        }
        for n in [30u32, 40] {
            tokio::fs::write(root.join("s30").join(format!("a{n}.m4s")), b"seek")
                .await
                .unwrap();
        }
        let read = |name: &'static str| {
            let root = root.to_path_buf();
            async move {
                let (p, _start) = HlsSegmentCache::resolve_audio_file(&root, name)
                    .await
                    .unwrap();
                tokio::fs::read(p).await.unwrap()
            }
        };
        // Below the seek session's start it cannot apply.
        assert_eq!(read("a5.m4s").await, b"from0");
        // At and above it, the deeper session wins — and keeps winning, so a
        // client playing on from a seek never alternates between two sessions'
        // incompatible cut points mid-playback.
        assert_eq!(read("a30.m4s").await, b"seek");
        assert_eq!(read("a40.m4s").await, b"seek");
        // Progress spans every session, so a seek session's output is visible
        // to the read wait.
        assert_eq!(
            HlsSegmentCache::audio_session_progress(td.path()).await,
            Some(40)
        );
        assert!(HlsSegmentCache::resolve_audio_file(td.path(), "a999.m4s")
            .await
            .is_none());
    }

    /// B106 — a fresh mid-file audio-track switch (new `-a{idx}` dir, no
    /// running session) must spawn a SEEK session at the playhead, not the
    /// whole-file from-0 re-encode. The old `want_seg <= LOOKAHEAD => 0` rule
    /// meant any switch inside the first ~120 s waited for a full 0→playhead
    /// Opus re-encode over NFS — the "incredibly long swap" symptom.
    #[test]
    fn shallow_switch_seeks_to_playhead_not_from_zero() {
        // want_seg=15 (90 s in), nothing running yet → seek AT 15, not 0.
        assert_eq!(
            HlsSegmentCache::choose_audio_start_seg(15, false, None),
            AudioStart::Start(15)
        );
    }

    #[test]
    fn play_from_start_uses_whole_file_from_zero_session() {
        assert_eq!(
            HlsSegmentCache::choose_audio_start_seg(0, false, None),
            AudioStart::Start(0)
        );
    }

    #[test]
    fn sequential_early_play_reuses_running_from_zero_session() {
        // from-0 session already running; a near-start segment lands during
        // its sequential write → reuse, don't spawn a redundant seek session.
        assert_eq!(
            HlsSegmentCache::choose_audio_start_seg(3, true, None),
            AudioStart::Reuse
        );
    }

    #[test]
    fn deep_seek_past_running_from_zero_spawns_seek_session() {
        // B42 — from-0 crawls sequentially; a deep want must not stall waiting
        // for it. A running seek session at 30 doesn't cover 100 either.
        assert_eq!(
            HlsSegmentCache::choose_audio_start_seg(100, true, Some(30)),
            AudioStart::Start(100)
        );
    }

    #[test]
    fn segment_within_seek_session_lookahead_is_reused() {
        assert_eq!(
            HlsSegmentCache::choose_audio_start_seg(35, false, Some(30)),
            AudioStart::Reuse
        );
    }

    // The high-severity VP9 seek fix: audio_hls_file must WAIT while a cold
    // session is still producing, not 404 on a fixed 5 s cliff. Parameterised
    // budgets keep these sub-second.

    #[tokio::test]
    async fn audio_hls_file_waits_for_a_segment_produced_after_a_delay() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024);
        let dir = td.path().join("s");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let write_dir = dir.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            tokio::fs::write(write_dir.join("a3.m4s"), b"seg3")
                .await
                .unwrap();
        });
        // no_progress budget (0.5 s) covers the 150 ms cold window.
        let got = cache
            .audio_hls_file_budget(&dir, "a3.m4s", 40, 10, 6)
            .await
            .unwrap();
        assert_eq!(got.bytes, b"seg3");
    }

    #[tokio::test]
    async fn audio_hls_file_keeps_waiting_while_the_session_advances() {
        // A session producing a3, a4, a5 over time must not be abandoned at the
        // stall budget: each new segment resets the stall counter, so the target
        // a5 (300 ms out, well past the 0.3 s stall window) is still served.
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024);
        let dir = td.path().join("s");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a2.m4s"), b"x").await.unwrap();
        let wd = dir.clone();
        tokio::spawn(async move {
            // Write a3, a4, a5 at ~80 ms increments — each gap is under the
            // 0.3 s stall budget, so progress keeps resetting the stall counter
            // and the target a5 (~240 ms out) is still served.
            for seg in [3u32, 4, 5] {
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                tokio::fs::write(wd.join(format!("a{seg}.m4s")), b"y")
                    .await
                    .unwrap();
            }
        });
        let got = cache
            .audio_hls_file_budget(&dir, "a5.m4s", 200, 10, 6)
            .await
            .unwrap();
        assert_eq!(got.bytes, b"y");
    }

    #[tokio::test]
    async fn audio_hls_file_gives_up_when_session_never_starts() {
        // Empty dir, nothing ever produced → NotFound after the no-progress
        // grace, not a 30 s hang.
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024);
        let dir = td.path().join("s");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let res = cache.audio_hls_file_budget(&dir, "a3.m4s", 200, 6, 6).await;
        // The give-up branch is part of the contract, not just "an error":
        // "never started" and "stalled" want opposite fixes.
        assert!(matches!(
            res,
            Err(HlsCacheError::AudioNotReady {
                reason: AudioWaitGiveUp::NeverStarted,
                last_progress: None,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn audio_hls_file_gives_up_after_a_producing_session_stalls() {
        // The session produced a2 then wedged; the target a9 never appears →
        // give up after the stall budget (not the full max).
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024);
        let dir = td.path().join("s");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a2.m4s"), b"x").await.unwrap();
        let res = cache
            .audio_hls_file_budget(&dir, "a9.m4s", 200, 100, 6)
            .await;
        // Distinct from the never-started case, and it must name what the
        // session HAD reached — "a2" is what tells you it was alive.
        assert!(matches!(
            res,
            Err(HlsCacheError::AudioNotReady {
                reason: AudioWaitGiveUp::Stalled,
                last_progress: Some(2),
                ..
            })
        ));
    }

    #[test]
    fn an_audio_free_segment_is_not_keyed_by_the_audio_track() {
        // A VP9 / h264-CMAF segment carries NO audio (`AudioDelivery::Separate`
        // → the argv ends in `-an`), so two requests differing only in the
        // client's AudioStreamIndex are the SAME bytes. Keying them apart minted
        // a second, byte-identical video ladder the moment a viewer switched
        // audio track, doubling GPU + NFS load mid-playback and dragging the
        // encoder below realtime (Ghost in the Shell, 2026-07-25: 6 of 47
        // segments encoded twice, each pair byte-for-byte equal).
        let opts = ident_opts(
            Some(SegmentVideo::Vp9),
            None,
            SegmentContainer::Fmp4,
            Some(4_000_000),
            None,
        );
        let track0 = SegmentIdentity::new(1, 7, Some(0), None, &opts);
        let track1 = SegmentIdentity::new(1, 7, Some(1), None, &opts);
        assert_eq!(track0, track1);
        assert_eq!(track0.filename(), track1.filename());
        assert_eq!(track0.etag(), track1.etag());
    }

    #[test]
    fn a_muxed_segment_is_still_keyed_by_the_audio_track() {
        // The converse must hold: an mpegts segment DOES carry the chosen
        // track's audio, so collapsing its key would serve one track's bytes
        // under another's request.
        let opts = ident_opts(
            Some(SegmentVideo::H264),
            Some(SegmentAudio::Aac),
            SegmentContainer::Mpegts,
            Some(4_000_000),
            Some(128_000),
        );
        assert_ne!(
            SegmentIdentity::new(1, 7, Some(0), None, &opts),
            SegmentIdentity::new(1, 7, Some(1), None, &opts)
        );
    }

    #[tokio::test]
    async fn a_fresh_seek_session_still_gets_the_cold_start_grace() {
        // A deep seek spawns `s40` while the finished from-0 session sits at
        // a12. Under the old GLOBAL high-water mark the wait saw Some(12),
        // never advancing, and burned the short stall budget while s40 was
        // still opening the source over NFS — so the segment 404'd even though
        // its session was healthy and about to write it. Progress must read as
        // "not started" until the session that will serve this request has
        // produced something.
        let td = TempDir::new().unwrap();
        let root = td.path().join("r");
        tokio::fs::create_dir_all(root.join("s40")).await.unwrap();
        tokio::fs::write(root.join("a12.m4s"), b"x").await.unwrap();

        assert_eq!(
            HlsSegmentCache::audio_wait_progress(&root, "a41.m4s").await,
            None,
            "s40 has written nothing — this request's session is still cold"
        );
        // Once s40 produces, the wait sees it and stall-detection applies.
        tokio::fs::write(root.join("s40").join("a40.m4s"), b"y")
            .await
            .unwrap();
        assert_eq!(
            HlsSegmentCache::audio_wait_progress(&root, "a41.m4s").await,
            Some(40)
        );
        // A session seeked PAST the target can never serve it, so it must not
        // keep that request's wait alive.
        assert_eq!(
            HlsSegmentCache::audio_wait_progress(&root, "a5.m4s").await,
            Some(12),
            "only the from-0 session can serve a5"
        );
    }

    #[test]
    fn the_stall_budget_outlasts_a_realtime_audio_session() {
        // The stall detector must never fire on a session that is merely SLOW.
        // A continuous audio session emits one segment at a time, so at 1×
        // realtime the gap between writes is one segment duration; the budget
        // has to clear that with margin, or ordinary I/O contention reads as a
        // wedge and 404s a live session.
        let stall_ms = HlsSegmentCache::AUDIO_POLL_STALL as f64
            * HlsSegmentCache::AUDIO_POLL_INTERVAL_MS as f64;
        assert!(
            stall_ms > pharos_core::segment_grid::SEGMENT_SECONDS * 1000.0,
            "stall budget {stall_ms}ms must exceed one segment duration"
        );
        // And it must still leave the overall cap room for several windows, so
        // a genuinely dead session fails inside 30 s rather than at it.
        const {
            assert!(HlsSegmentCache::AUDIO_POLL_STALL * 2 < HlsSegmentCache::AUDIO_POLL_MAX);
        }
    }

    #[test]
    fn audio_wait_give_up_labels_are_stable_and_distinct() {
        let all = [
            AudioWaitGiveUp::NeverStarted,
            AudioWaitGiveUp::Stalled,
            AudioWaitGiveUp::BudgetExhausted,
        ];
        let labels: Vec<&str> = all.iter().map(|r| r.label()).collect();
        assert_eq!(labels, vec!["never_started", "stalled", "budget_exhausted"]);
        let mut uniq = labels.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), labels.len(), "labels must be distinct");
        // "served" shares the label space and must not collide with a give-up.
        assert!(!labels.contains(&"served"));
    }

    #[test]
    fn audio_not_ready_names_the_cause_not_just_the_class() {
        let e = HlsCacheError::AudioNotReady {
            name: "a48.m4s".into(),
            reason: AudioWaitGiveUp::Stalled,
            waited_ms: 3000,
            last_progress: Some(41),
        };
        let msg = e.to_string();
        assert!(msg.contains("a48.m4s"), "{msg}");
        assert!(msg.contains("stopped advancing"), "{msg}");
        assert!(msg.contains("3000ms"), "{msg}");
        assert!(msg.contains("a41.m4s"), "{msg}");
    }
}
