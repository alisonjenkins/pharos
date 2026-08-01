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
//!   later requester for the same key wait on a lock (B108). The encode
//!   outlives the requester that started it but NOT the last requester
//!   wanting it: while the encode is still QUEUED, a driver whose receivers
//!   have all gone stops (`await_last_requester_gone`), so an episode swap
//!   reclaims the previous episode's queued speculative encodes (V58, PR #75).
//!   What it does not reclaim is a DISPATCHED encode — that one holds a worker
//!   and a device permit and runs to completion whatever anyone does, so the
//!   driver stays with it and keeps the bytes.
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

use pharos_transcode::scheduler::{JobClass, JobHint, PlayheadSeed, StreamKey};
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
    /// This request ran NO encode of its own and got no segment: either it
    /// coalesced onto an in-flight encode for the same key which then failed,
    /// or the driver it was waiting on went away before `produce_segment` ever
    /// ran (a panic, or runtime shutdown) — a case nothing counted, because the
    /// encode that would have counted itself never started. Kept apart from
    /// `Failed` because they are different denominators — `Failed` counts
    /// production attempts, this counts requests taken down BY one — and the
    /// ratio between them is the blast radius of a single bad encode.
    CoalescedFailed,
    /// This request ran no encode of its own and inherited a `SchedulerBusy`
    /// from the job it joined: deliberate load-shedding, decided about somebody
    /// else's submission, reaching a client.
    ///
    /// NOT `CoalescedFailed`, and the distinction is the whole reason this
    /// value exists. Under a prefetch storm — the B108 condition — Background
    /// waiters are shed en masse, so folding these into the failure bucket
    /// would make `failed + coalesced_failed` (the sum V91 tells an operator to
    /// alert on) dominated by `reason="scheduler_busy"`, i.e. alerting on the
    /// admission control working as designed. A shed is a decision; a failure
    /// is a fault. `shed + coalesced_shed` is the shedding total, disjoint from
    /// the failure total.
    CoalescedShed,
    /// Every requester for this segment went away before it was produced, so
    /// the encode was stopped rather than finished.
    ///
    /// Not a failure and not a shed: nothing went wrong and nothing was
    /// refused — the work stopped being wanted, which is what an episode swap
    /// or a `DELETE /Videos/ActiveEncodings` does to a window of prefetch. It is
    /// the arm that keeps `pharos_segment_produced_total` a partition of
    /// production attempts (V128) now that an attempt can end this way.
    ///
    /// It counts ABANDONMENTS, not capacity returned, and the two are different
    /// numbers. An encode is only stopped while its job is still QUEUED — past
    /// dispatch the worker owns a device permit and finishes regardless
    /// (`JobSlot::is_dispatched`) — so this says how many encodes never started,
    /// which is what PR #75 existed to prevent and could never be shown
    /// preventing. GPU seconds handed back is a different question, and
    /// `pharos_transcode_queue_outcome_total{outcome="abandoned"}` is nearer to
    /// it.
    Cancelled,
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
            Self::CoalescedFailed => "coalesced_failed",
            Self::CoalescedShed => "coalesced_shed",
            Self::Cancelled => "cancelled",
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

/// Count one request that failed because the encode it COALESCED ONTO failed.
///
/// The success half of the coalescing path records a hit per waiter; the failure
/// half recorded nothing, so ONE failed encode returned to N waiters incremented
/// `pharos_segment_produced_total` exactly once. That counter is what a
/// segment-failure alert reads, so it undercounted client-visible failures by
/// the coalescing factor, and nothing anywhere said how many requests a single
/// failed encode took down — the rich-on-one-path, silent-on-the-other shape, at
/// the one place where a single fault reaches an unknown number of clients.
///
/// Deliberately a distinct `outcome` on the SAME counter rather than another
/// `failed` or a parallel metric: `failed` counts production attempts and this
/// counts requests taken down by one, so summing them gives the client-visible
/// total while their ratio gives the blast radius. `reason` is `failure_reason`
/// of the inherited error, so the two share one vocabulary and no new label —
/// and no new cardinality — is introduced.
///
/// Two situations reach here, and they share this outcome because they share the
/// property that makes the count necessary — no encode of this request's own ran,
/// so nothing else counted it: a published failure from a driver this request
/// joined, and a driver that vanished before `produce_segment` started at all
/// (panic or runtime shutdown), which is uncounted even for the requester that
/// spawned it. What does NOT reach here is an inherited `SchedulerBusy`; see
/// [`record_coalesced_shed`], because a load-shed decision is not a fault and
/// must stay out of the failure sum V91 tells operators to alert on.
///
/// The line beside it carries the same identity fields as the hit line, at
/// `warn`: the error is real, but it is somebody else's error, so it must not be
/// mistaken for a second failing encode in the log.
fn record_coalesced_failure(
    media_id: u64,
    seg_index: u32,
    opts: &SegmentOpts,
    class: JobClass,
    err: &HlsCacheError,
) {
    let reason = failure_reason(err);
    record_segment_failure(SegmentOutcome::CoalescedFailed, reason, class);
    tracing::warn!(
        media.id = media_id,
        seg = seg_index,
        class = class.label(),
        reason,
        error = %err,
        codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
        burn = opts.burn_subtitle_stream_index.is_some(),
        burn_idx = opts.burn_subtitle_stream_index,
        audio_idx = opts.audio_source_stream_index,
        seek_secs = opts.window.start_seconds(),
        "hls segment request failed with the encode it coalesced onto"
    );
}

/// Count one request that inherited a LOAD-SHED decision made about another
/// job's submission.
///
/// This is client-visible — the request returns `SchedulerBusy` and the browser
/// sees a 500 on a video segment — so it must be counted; the mistake it exists
/// to undo is counting it as an encode FAILURE. `failure_reason(SchedulerBusy)`
/// is `"scheduler_busy"`, so before this outcome existed every one of these
/// landed as `{outcome="coalesced_failed", reason="scheduler_busy"}`. Under a
/// prefetch storm — precisely the B108 condition, where Background jobs are shed
/// by design — that reason dominates the bucket, and an operator following V91's
/// own arithmetic (`failed + coalesced_failed`) alerts on deliberate
/// load-shedding as breakage: the same shed/failure conflation the coalescing
/// fall-through exists to keep off the RESPONSE path, reappearing one signal
/// layer down.
///
/// Same counter, same `outcome` label key, one more bounded value, `reason`
/// still from the `failure_reason` vocabulary: no new label key and no new
/// cardinality. `warn`, not `debug` as the driving requester's own shed is
/// logged, because a shed a client asked for is a declined guess while this one
/// is a declined guess a client is BLOCKED ON.
fn record_coalesced_shed(media_id: u64, seg_index: u32, opts: &SegmentOpts, class: JobClass) {
    record_segment_failure(SegmentOutcome::CoalescedShed, "scheduler_busy", class);
    tracing::warn!(
        media.id = media_id,
        seg = seg_index,
        class = class.label(),
        reason = "scheduler_busy",
        codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
        burn = opts.burn_subtitle_stream_index.is_some(),
        burn_idx = opts.burn_subtitle_stream_index,
        audio_idx = opts.audio_source_stream_index,
        seek_secs = opts.window.start_seconds(),
        "hls segment request shed by the admission decision made about the encode it coalesced onto"
    );
}

/// Count one V127 re-drive: a requester declining an admission decision made
/// about somebody else's job and driving its own encode instead.
///
/// This is the SAFETY VALVE for a defect that reached a viewer as a 500 on a
/// video segment (B134): a client coalescing onto a speculative driver and
/// inheriting its `SchedulerBusy`. It was `debug!`-only, which at the
/// deployment's log level is invisible — so the one signal that says how often
/// the valve is load-bearing could not be read at all, and neither could its
/// disappearance if a refactor stopped the fall-through firing.
///
/// A COUNTER of its own rather than another `pharos_segment_produced_total`
/// arm, and deliberately so. A re-drive produces no outcome yet — the second
/// attempt does, and records itself — so folding it into that counter would
/// break the partition V128 requires by double-counting one request. (The
/// objection once raised against instrumenting this, that it would corrupt
/// `outcome="shed"`, was already answered on this branch by `coalesced_shed`:
/// the answer is a distinct series, not silence.)
///
/// `class` is the RE-DRIVING requester's own — two values, bounded.
fn record_redrive(media_id: u64, seg_index: u32, class: JobClass) {
    metrics::counter!(
        "pharos_segment_redrive_total",
        "class" => class.label(),
    )
    .increment(1);
    tracing::debug!(
        media.id = media_id,
        seg = seg_index,
        class = class.label(),
        "hls segment: re-driving rather than inheriting another job's shed"
    );
}

/// Which counter, if any, a failed wait on the shared registry belongs to.
///
/// One decision in one place, because V91 tells an operator to SUM these and
/// every wrong merge here becomes a wrong number in front of somebody on call.
/// `None` means "already counted" — not "ignore".
///
/// - **The driver went away** before `produce_segment` ever ran (panic, runtime
///   shutdown). The encode that would have counted itself never started, so this
///   request is counted NOWHERE else — and that is equally true of the requester
///   that SPAWNED the driver, which is why this arm is decided before `driving`.
///   Reuses `coalesced_failed` rather than inventing a third value: the property
///   that makes the count necessary is "no encode of this request's own ran",
///   which is exactly what that outcome counts.
/// - **This request drove the encode** and the driver published. `produce_segment`
///   already recorded `failed` or `shed` for it; counting again double-counts one
///   production attempt.
/// - **Somebody else's published outcome.** A failure is `coalesced_failed`; a
///   `SchedulerBusy` is `coalesced_shed`, because deliberate load-shedding is a
///   decision and not a fault, and folding it into the failure bucket makes the
///   V91 sum alert on admission control doing its job under a prefetch storm.
fn shared_failure_outcome(
    err: &HlsCacheError,
    driving: bool,
    driver_gone: bool,
) -> Option<SegmentOutcome> {
    match (driving, driver_gone) {
        (_, true) => Some(SegmentOutcome::CoalescedFailed),
        (true, false) => None,
        (false, false) => match err {
            HlsCacheError::SchedulerBusy => Some(SegmentOutcome::CoalescedShed),
            _ => Some(SegmentOutcome::CoalescedFailed),
        },
    }
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
    /// Source of [`InFlightSegment::id`]. One registration is not
    /// interchangeable with the next for the same key, and two places have to
    /// be able to say so under the map's shard lock: the driver's deregistration
    /// (which must never remove a SUCCESSOR's entry) and the abandonment check
    /// beside it.
    next_registration: Arc<std::sync::atomic::AtomicU64>,
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

/// How one wait on the shared registry ended.
///
/// The two arms are not interchangeable FOR ACCOUNTING, which is the only reason
/// this is not just a [`SharedSegment`]. A published error was already counted by
/// the encode that produced it (`produce_segment` records `failed` / `shed`
/// before it returns), so a driving requester must not count it again. A driver
/// that went away never reached `produce_segment` at all, so nothing counted it
/// and the requester is the only party left that can — driving or not.
enum SegmentWait {
    /// The driver published an outcome, success or failure.
    Published(SharedSegment),
    /// Every sender is gone and nothing was published: the driver panicked, or
    /// the runtime is shutting down.
    DriverGone,
}

impl SegmentWait {
    /// The error a caller sees when its driver went away. Names the two causes,
    /// because "no result" with no reason is the shape that costs an hour.
    fn driver_gone_error() -> Arc<HlsCacheError> {
        Arc::new(HlsCacheError::Transcode(
            "segment encode driver stopped without publishing a result \
             (the task panicked or the runtime is shutting down)"
                .to_string(),
        ))
    }

    fn into_outcome(self) -> SharedSegment {
        match self {
            Self::Published(v) => v,
            Self::DriverGone => Err(Self::driver_gone_error()),
        }
    }
}

/// A segment somebody is already producing.
#[derive(Clone)]
struct InFlightSegment {
    /// Distinguishes this registration from any later one for the same key.
    /// Compared under the map's shard lock, so a driver can only ever
    /// deregister ITSELF.
    id: u64,
    /// The driver's publish channel. The registry holds the SENDER and no
    /// receiver, and that is load-bearing rather than incidental: it makes
    /// `receiver_count()` exactly the number of REQUESTERS still waiting on
    /// these bytes, so the driver can be told when the last of them has gone.
    /// A receiver parked here for bookkeeping would sit in that count forever,
    /// and the driver could never tell "somebody is waiting" from "nobody is".
    ///
    /// A joiner therefore `subscribe()`s rather than cloning a stored receiver;
    /// a requester holds only its own receiver, so a requester going away is
    /// visible here while a requester can still be woken by the sender's
    /// disappearance ([`SegmentWait::DriverGone`]).
    tx: Arc<tokio::sync::watch::Sender<Option<SharedSegment>>>,
    /// Who the driver submitted as. A joiner that is blocked on these bytes
    /// while the driver is speculative has to say so — the outcome is shared,
    /// but the driver's PRIORITY was decided about the driver (V127), and a
    /// client silently adopting a prefetch's tier is the whole hazard.
    driver_class: JobClass,
    /// Names the driver's scheduler job once it has one, so a joiner can ask
    /// for it to be promoted. Empty until the driver actually submits, and a
    /// receiver only — holding the sender here would keep a dead driver's slot
    /// open and hang every waiter on it.
    job: tokio::sync::watch::Receiver<Option<pharos_transcode::protocol::JobId>>,
    /// Set the moment a client joins a speculative driver.
    ///
    /// Promotion re-ranks the JOB, and a retry is a different job: the second
    /// `-progress`-completeness attempt re-submits under a new id, and joiners
    /// that already promoted the first one will not promote again. Without this
    /// the client silently drops back to the speculative tier for that segment,
    /// with nothing recorded to say so. The driver reads it to choose the class
    /// it re-submits as, which is why the flag lives beside the registration
    /// rather than inside the promotion task: the fact that a client is waiting
    /// outlives the individual job it was waiting on.
    promoted: Arc<std::sync::atomic::AtomicBool>,
}

/// Rebuild an owned error for a waiter out of the one the driver produced.
///
/// The variant is preserved because the code that reads it ACTS on it, and a
/// `String` would have destroyed that: `SchedulerBusy` is shedding rather than
/// breakage, so the coalescing fall-through in `segment_bytes_keyed` declines to
/// adopt it and `shared_failure_outcome` counts it apart from a failure. Note
/// what is NOT true of this function's output — `failure_reason` does not label
/// metrics from it. It runs on the ORIGINAL error, inside `produce_segment` for
/// the encode's own outcome and inside `record_coalesced_failure` on the shared
/// `Arc<HlsCacheError>`; nothing calls it on a rebuilt copy. The `Io` arm keeps
/// the kind and the full message; only the OS error's raw code object is left
/// behind, which nothing here reads.
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

/// What one call to [`HlsSegmentCache::register_or_join`] got.
struct Registration {
    /// Where this requester waits for the outcome. The ONLY receiver it holds,
    /// so this requester going away is visible to the driver.
    rx: tokio::sync::watch::Receiver<Option<SharedSegment>>,
    /// Whether THIS call is the one driving the encode.
    driving: bool,
    /// Which registration was joined, so a later look at the map can tell this
    /// one from a successor for the same key.
    id: u64,
}

/// Resolve when the last REQUESTER of `key` has gone, deregistering `key` in
/// the same breath.
///
/// This is what makes `PrefetchRegistry`'s `h.abort()` mean something again.
/// Detaching the driver was right for the case where somebody else is still
/// waiting for the bytes and wrong for the case where nobody is: aborting a
/// prefetch task killed only the WAITER, the scheduler's `oneshot` stayed alive
/// inside the detached driver, so `reply.is_closed()` was false for every
/// abandoned prefetch and `reap_abandoned` / `QueueOutcome::Abandoned` were
/// near-dead for the case they were written for. An episode swap left roughly
/// 6-14 orphaned encodes for the previous episode draining onto the GPU while
/// the new episode was starting, and V58's claim that "a seek or a track swap
/// closes its `oneshot` and the abandonment sweep collects it" had quietly
/// become false. This is PR #75's fix, re-made against the shared-result design.
///
/// `Sender::closed()` is the signal precisely because the registry holds no
/// receiver: every receiver in existence belongs to a requester, so zero
/// receivers means nobody is waiting. It is awaited in a LOOP because a joiner
/// may `subscribe` after it resolves, reopening the channel.
///
/// The race that would otherwise make this unsafe — a joiner arriving in the
/// instant between the wake and the abandonment — is closed by taking the
/// decision UNDER the map's shard lock. `remove_if` and a joiner's `entry()`
/// contend for the same lock, so either the joiner subscribed first (the
/// predicate sees its receiver, declines, and the encode carries on) or it will
/// not find the entry at all and drives a fresh one. There is no interleaving
/// in which a live requester is left waiting on an encode that has decided to
/// stop.
///
/// It stops being armed at DISPATCH, and that boundary is the point of
/// `JobSlot::is_dispatched`. Abandoning a queued job hands its capacity back;
/// abandoning a running one hands nothing back — `spawn_run_task` is detached
/// and owns the worker and the permit, and the only `reply.is_closed()` reads in
/// the scheduler are pre-dispatch. So past dispatch the choice is not "stop the
/// encode or let it run", it is "let it run and keep the bytes, or let it run
/// and throw them away". That alone would settle it, and there is worse: the
/// orphan keeps writing to `{seg}.ts.tmp` and its `-progress` sidecar, both
/// derived from the KEY, so the next requester for the same key starts a second
/// worker on the same two files. `read_progress` consumes the sidecar and
/// `short_of_frames(None, _)` fails OPEN, so the completeness gate can be
/// defeated by the collision and `rename` then publishes an inode the orphan is
/// still writing into — a corrupt segment, cached, served with a 200 for as long
/// as it stays on disk. Ending the region at dispatch costs zero reclaim and
/// removes that shape.
///
/// `published` is the same boundary for the path that has no scheduler (the
/// inline transcoder, whose encode really does die with the future): once the
/// segment has been renamed into the cache path, abandoning would count one
/// attempt twice — `Ok` and then `Cancelled`, breaking V128's partition — and
/// would leave a published `.ts` that `record` never saw, so the LRU under-counts
/// `total_bytes` permanently and never evicts it.
async fn await_last_requester_gone(
    tx: &tokio::sync::watch::Sender<Option<SharedSegment>>,
    map: &DashMap<SegmentIdentity, InFlightSegment>,
    key: SegmentIdentity,
    id: u64,
    job: &pharos_transcode::scheduler::JobSlot,
    published: &std::sync::atomic::AtomicBool,
) {
    loop {
        tx.closed().await;
        if job.is_dispatched() || published.load(std::sync::atomic::Ordering::Acquire) {
            // Past reclaiming: never resolve again, so the `select!` can only
            // end by the encode finishing. Not a `return` — that IS the
            // cancellation — and not a `break` out of the loop either, because
            // resolving at all drops the produce future.
            std::future::pending::<()>().await;
        }
        // Three outcomes, told apart under the ONE shard lock. The loop exists
        // for exactly one of them.
        let mut ours_but_awaited = false;
        let removed = map.remove_if(&key, |_, v| {
            if v.id != id {
                // A successor's registration. Not ours to remove — that is what
                // the id qualification is for.
                return false;
            }
            if v.tx.receiver_count() == 0 {
                return true;
            }
            ours_but_awaited = true;
            false
        });
        if removed.is_some() {
            return;
        }
        if !ours_but_awaited {
            // The entry is gone, or it belongs to somebody else. Either way this
            // driver has no registration left, so nobody can coalesce onto it
            // and there is nothing to wait for. Looping would re-await a
            // `closed()` that is ALREADY resolved (our own receiver count is
            // zero) and re-take a decision that cannot change — an infinite
            // loop with no await point in it, inside one poll, pegging a
            // runtime worker and wedging this task for the life of the process.
            // Only the id-qualified guard and this function remove entries in
            // production, so today the loop cannot be entered that way; this is
            // one `if` between "unreachable" and "impossible".
            return;
        }
    }
}

/// Where ONE REGISTRATION writes its segment before publishing it.
///
/// Qualified by the registration id, not by the key alone, so two drivers for
/// the same key can never write the same file. That sounds impossible — a key
/// has one registration at a time — and it is impossible in every ordinary
/// interleaving. It is not impossible at the seam where a driver abandons:
/// `await_last_requester_gone` ends its cancellable region at
/// `JobSlot::is_dispatched`, and that flag is set by the scheduler in the same
/// synchronous step as `spawn_run_task`, so there is a window — the width of an
/// atomic load and a `remove_if` — in which a driver reads "not dispatched",
/// deregisters, and drops its future while the actor is dispatching that very
/// job. The scheduler's own `reply.is_closed()` check has the mirror-image
/// window and cannot close it either; closing it properly would need the two
/// decisions to exclude each other, which is a redesign of the dispatch path.
///
/// So the collision is made harmless instead of merely unlikely. The orphan
/// writes to a name its successor will never choose, `read_progress` reads the
/// sidecar its own encode wrote, and the completeness gate cannot be defeated by
/// somebody else's file. What is left is one abandoned tmp file per crossing —
/// unreferenced, never renamed, never served — which is the trade this makes on
/// purpose: a leaked temp file is recoverable and a corrupt cached segment
/// served with a 200 is not.
fn segment_tmp_path(path: &Path, registration: u64) -> PathBuf {
    path.with_extension(format!("{registration}.ts.tmp"))
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
    /// Remove OUR registration and no other. A driver that abandons its encode
    /// deregisters early — under the shard lock, so a fresh driver may already
    /// have registered this key by the time the guard runs — and an unqualified
    /// `remove` would then delete a live successor's entry, leaving its
    /// requesters waiting on a channel nothing can publish to: exactly the state
    /// this guard exists to prevent, produced by the guard itself.
    id: u64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.map.remove_if(&self.key, |_, v| v.id == self.id);
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
// 16 (spec 007): the assignment rule moves a FOURTH time. Every supporting
// device now enters the rendition pool weighted by a boot probe, where before
// any hardware device shut software out of every codec it could encode — so a
// rendition whose weight band changed resolves to a different encoder than the
// one that produced its cached segments. By V89 that makes every artefact
// cached under 15 stale, for the reason the 13 and 15 bumps already record: the
// deploy that changes the rule is itself the moment the rule changes, and
// `SegmentIdentity` carries no device, so nothing downstream can tell a
// libx264 segment from an NVENC one until a browser fails to decode it.
//
// The price is one cold cache after this deploy. The alternative is #114 fired
// once per moved rendition during the transition, delivered as a 200.
/// Public because the HLS playlists embed it in every init/segment URI: a
/// browser caches those `immutable` for a year, so a generation change must
/// change the URL or clients keep serving themselves the previous
/// generation's init (see `hls::rendition_qs`). That property is what makes
/// this re-land safe where the first attempt was not: the bump is now visible
/// to the CLIENT cache, not only to the server's.
pub const HLS_GEN_VERSION: u32 = 16;
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
            next_registration: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        // No play session reaches this legacy wrapper (see its own doc
        // comment) — nothing is known about who is asking, so this stream's
        // work sorts last, same as any other unattributed job.
        self.segment_bytes_keyed(
            media_id,
            seg_index,
            None,
            None,
            source,
            opts,
            class,
            StreamKey::NONE,
            PlayheadSeed::Observes,
        )
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
    ///
    /// `seeds_playhead` says whether this caller KNOWS where `stream`'s playback
    /// begins — [`PlayheadSeed::StatesTheStart`] for the two prewarms, which
    /// pick their base from the resume position or the group's seek target, and
    /// [`PlayheadSeed::Observes`] for everything else. It cannot be inferred
    /// down in the scheduler from the playhead map being empty, because THIS
    /// function returns on the fast cache-hit path below without ever reaching
    /// the scheduler: a session whose opening segments are already on disk has
    /// no playhead entry when its first deep prefetch is submitted.
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
        stream: StreamKey,
        seeds_playhead: PlayheadSeed,
    ) -> Result<Vec<u8>, HlsCacheError> {
        let hint = JobHint {
            stream,
            segment: Some(seg_index),
            seeds_playhead,
        };
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
                    self.note_client_playhead(class, stream, seg_index);
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
        let (outcome, driving, driver_gone) = loop {
            attempt += 1;
            let Registration {
                mut rx,
                driving,
                id: registration_id,
            } = self.register_or_join(key, source, opts, media_id, seg_index, class, hint);
            let wait = Self::await_segment(&mut rx).await;
            let driver_gone = matches!(wait, SegmentWait::DriverGone);
            let outcome = wait.into_outcome();
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
            // else's job (V127: share a RESULT, never a POLICY DECISION). A
            // DRIVING requester keeps its own shed (B108/V58): shedding work you
            // submitted yourself is the intended behaviour.
            let inherited_shed = !driving
                && matches!(&outcome, Err(e) if matches!(**e, HlsCacheError::SchedulerBusy));
            if !(inherited_shed && attempt == 1) {
                break (outcome, driving, driver_gone);
            }
            record_redrive(media_id, seg_index, class);
            // The driver publishes and only THEN drops its guard, so between
            // those two points the registration is a corpse: an entry whose
            // outcome is already decided. Re-registering onto it would hand
            // back the same shed and waste the one re-attempt. The window is a
            // few instructions wide and only reachable across threads, so one
            // `yield_now` is enough to let the guard run; if it somehow is not,
            // the second attempt inherits the shed and we are exactly where
            // this branch found us, never worse. Matched by registration id so
            // a NEW driver's registration is never mistaken for the corpse.
            let corpse_still_registered = self
                .inflight
                .get(&key)
                .is_some_and(|e| e.id == registration_id);
            if corpse_still_registered {
                tokio::task::yield_now().await;
            }
        };

        let bytes = match outcome {
            Ok(b) => b,
            Err(e) => {
                // Symmetry: the success half of this path records a hit per
                // waiter, so the failure half records one per waiter too — but
                // it must record the RIGHT thing, because V91 tells an operator
                // to SUM these. Which counter, and whether any, is
                // `shared_failure_outcome`'s single decision.
                match shared_failure_outcome(&e, driving, driver_gone) {
                    None => {}
                    Some(SegmentOutcome::CoalescedShed) => {
                        record_coalesced_shed(media_id, seg_index, opts, class);
                    }
                    Some(_) => record_coalesced_failure(media_id, seg_index, opts, class, &e),
                }
                return Err(shared_copy(&e));
            }
        };
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
            // `stream` is THIS requester's — the joiner's — which is the whole
            // care needed here. The driver's stream belongs to a viewer who
            // never asked for this segment (it is their prefetch, running ahead
            // of them), and declaring THEM to be standing on it is the bug
            // `JobHint::on_behalf_of_a_joiner` exists to prevent, arriving by
            // another route. A driving requester takes no branch: it did not get
            // a hit, and its own `Submit` already moved its reading.
            self.note_client_playhead(class, stream, seg_index);
        }
        Ok(bytes.as_ref().clone())
    }

    /// Tell the scheduler where this viewer has reached, for a segment the
    /// scheduler never saw a job for.
    ///
    /// Both hit paths land here, and only a hit does: a MISS reports itself,
    /// through the `Submit` that services it. Without this the reading is
    /// miss-driven, so a viewer whose buffer is warm — the healthiest stream on
    /// the box — stops updating it entirely, and every consumer of
    /// `lookahead_distance` degrades in proportion to how well prefetch is
    /// working.
    ///
    /// Interactive only. A speculative hit says where somebody GUESSED the
    /// viewer would be, which is the self-measurement `PlayheadSeed` forbids on
    /// the submit side; there is no reason to let it in through the cache. And
    /// the send itself never blocks or awaits — see
    /// [`TranscodeScheduler::note_playhead`] for why a lost update is safe.
    fn note_client_playhead(&self, class: JobClass, stream: StreamKey, seg_index: u32) {
        if class != JobClass::Interactive || stream == StreamKey::NONE {
            return;
        }
        if let Some(sched) = &self.scheduler {
            sched.note_playhead(stream, seg_index);
        }
    }

    /// Ask the scheduler to re-rank the driver of a segment a client is now
    /// waiting on.
    ///
    /// Detached, because the requester's next move is to wait for the bytes and
    /// nothing it does depends on the promotion landing. It waits for the
    /// driver to be ASSIGNED an id rather than reading one and giving up: a
    /// joiner routinely arrives while the driver is still resolving audio, and
    /// giving up there would leave exactly the client this exists for waiting at
    /// a speculative tier. The wait ends by itself if the driver never submits
    /// — the slot's sender dies with it.
    fn promote_driver(
        &self,
        mut job: tokio::sync::watch::Receiver<Option<pharos_transcode::protocol::JobId>>,
    ) {
        let Some(sched) = self.scheduler.clone() else {
            return;
        };
        tokio::spawn(async move {
            match pharos_transcode::scheduler::await_job_id(&mut job).await {
                Some(id) => sched.promote(id).await,
                // Counted here rather than in the scheduler: the scheduler
                // never hears about this one, and a promotion with no target is
                // precisely the window in which a client can still inherit a
                // speculative tier.
                None => pharos_transcode::scheduler::PromotionOutcome::Unassigned.record(),
            }
        });
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
        hint: JobHint,
    ) -> Registration {
        let job_slot = pharos_transcode::scheduler::JobSlot::new();
        let promoted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let id = self
            .next_registration
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (rx, registration_id, tx, joined) = match self.inflight.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                let e = e.get();
                (
                    // `subscribe`, not a clone of a stored receiver: the registry
                    // deliberately holds none, so every receiver that exists
                    // belongs to a requester.
                    e.tx.subscribe(),
                    e.id,
                    None,
                    Some((e.driver_class, e.job.clone(), e.promoted.clone())),
                )
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                let tx = Arc::new(tx);
                e.insert(InFlightSegment {
                    id,
                    tx: tx.clone(),
                    driver_class: class,
                    job: job_slot.subscribe(),
                    promoted: promoted.clone(),
                });
                // `rx` is this requester's own, taken before the driver is even
                // spawned — so the driver can never observe "no requesters"
                // before its own requester has one.
                (rx, id, Some(tx), None)
            }
        };
        if let Some((driver_class, job, promoted)) = joined {
            // A client has arrived behind somebody else's guess. Sharing the
            // RESULT is right and is the point; inheriting the DRIVER'S TIER is
            // not, and would leave this client ranked behind every other
            // client's work — including work submitted after it started waiting.
            if class == JobClass::Interactive && driver_class == JobClass::Background {
                // Recorded synchronously, before the promotion is even sent:
                // this says a client is waiting on these BYTES, which stays true
                // across however many jobs the driver needs to produce them.
                // The message re-ranks one job; the flag re-ranks the retry.
                promoted.store(true, std::sync::atomic::Ordering::Release);
                self.promote_driver(job);
            }
            return Registration {
                rx,
                driving: false,
                id: registration_id,
            };
        }
        // Only the Vacant arm reaches here, and it always produced a sender.
        let Some(tx) = tx else {
            return Registration {
                rx,
                driving: false,
                id: registration_id,
            };
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
            id,
        };
        // Carry this request's span into the detached task. Without it the
        // encode's own lines and its `write_segment` span land unparented, and
        // the miss line loses the request it belongs to — the trace would end
        // where the work begins.
        let span = tracing::Span::current();
        let map = self.inflight.clone();
        // Raised by the driver the instant its segment is renamed into the cache
        // path. See `await_last_requester_gone`: from there an abandonment would
        // count one attempt twice and orphan a file the LRU never learns about.
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_job = job_slot.clone();
        let cancel_published = published.clone();
        tokio::spawn(
            async move {
                // Named so the block CAPTURES it: registered until the driver
                // ends, panic included.
                let _guard = guard;
                let produce = driver.produce_segment(
                    &driver_source,
                    &driver_opts,
                    key,
                    media_id,
                    seg_index,
                    class,
                    hint,
                    job_slot,
                    promoted,
                    published,
                    id,
                );
                tokio::pin!(produce);
                tokio::select! {
                    out = &mut produce => {
                        // Publish BEFORE the guard removes the entry, so a
                        // requester that subscribed a moment ago always sees a
                        // value.
                        let _ = tx.send(Some(out.map(Arc::new).map_err(Arc::new)));
                    }
                    () = await_last_requester_gone(
                        &tx, &map, key, id, &cancel_job, &cancel_published,
                    ) => {
                        // Nobody is waiting for these bytes any more, and the
                        // registration is already gone — removed under the shard
                        // lock, so no joiner can attach to an encode that is
                        // about to stop. Dropping `produce` drops the
                        // scheduler's `submit()` future with it, closing that
                        // job's `oneshot`, which is exactly what `place` and
                        // `reap_abandoned` read to collect it as
                        // `QueueOutcome::Abandoned`. Reachable only while the
                        // job is still QUEUED — that is the whole population an
                        // abandonment can reclaim.
                        record_segment_outcome(SegmentOutcome::Cancelled, class);
                        // Usually there is nothing here: the job was queued, so
                        // no worker ever opened this path. Removed anyway
                        // because the crossing window `segment_tmp_path`
                        // describes can leave an orphan writing to it, and
                        // unlinking a file a worker still holds open costs
                        // nothing and reclaims its space when that worker exits.
                        let orphan = segment_tmp_path(&driver.segment_path_keyed(key), id);
                        let _ = tokio::fs::remove_file(progress_sidecar_path(&orphan)).await;
                        let _ = tokio::fs::remove_file(&orphan).await;
                        tracing::debug!(
                            media.id = media_id,
                            seg = seg_index,
                            class = class.label(),
                            "hls segment encode cancelled: every requester went away"
                        );
                    }
                }
            }
            .instrument(span),
        );
        Registration {
            rx,
            driving: true,
            id,
        }
    }

    /// Wait for the driver to publish this segment's outcome.
    ///
    /// Two iterations at most: the value is either already published or arrives
    /// with the next change, and the only value ever sent is `Some(_)`.
    async fn await_segment(
        rx: &mut tokio::sync::watch::Receiver<Option<SharedSegment>>,
    ) -> SegmentWait {
        loop {
            let published = rx.borrow_and_update().clone();
            if let Some(v) = published {
                return SegmentWait::Published(v);
            }
            if rx.changed().await.is_err() {
                // Every sender is gone and nothing was published: the driver
                // panicked, or the runtime is shutting down. Say so and let the
                // caller retry — the registration is already gone with it, so
                // the next request re-drives — rather than waiting forever on a
                // channel nobody can send to. Reported as its own arm rather
                // than as an error so the caller can tell it apart from a
                // failure the encode already counted.
                return SegmentWait::DriverGone;
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
    ///
    /// It does NOT outlive the last requester, UNTIL its encode is dispatched.
    /// While the job is still queued, the future this returns is dropped the
    /// moment no receiver for the segment remains
    /// (`await_last_requester_gone`), which drops the scheduler `submit()`
    /// inside it and hands the job to the abandonment sweep. Detaching without
    /// that made `PrefetchRegistry`'s abort a no-op against the encode itself.
    /// Once a worker has the job, the encode runs to completion whatever anyone
    /// does — see `JobSlot::is_dispatched` — so this future runs to completion
    /// too, and the bytes reach the cache instead of being thrown away by a
    /// cancellation that reclaimed nothing.
    #[allow(clippy::too_many_arguments)]
    async fn produce_segment(
        &self,
        source: &Path,
        opts: &SegmentOpts,
        key: SegmentIdentity,
        media_id: u64,
        seg_index: u32,
        class: JobClass,
        hint: JobHint,
        // `job_slot` is published to as soon as the scheduler names this
        // encode's job, so a requester coalescing onto it can have it promoted.
        // Owned here, so its sender dies with this task and no waiter outlives
        // it.
        job_slot: pharos_transcode::scheduler::JobSlot,
        // Set by `register_or_join` when a client coalesces onto this encode.
        // Read on the retry, whose submission is a NEW job that no joiner will
        // promote a second time.
        promoted: Arc<std::sync::atomic::AtomicBool>,
        // Raised at the rename, so the cancellation this future races against
        // cannot fire once the segment exists. See `await_last_requester_gone`.
        published: Arc<std::sync::atomic::AtomicBool>,
        // This registration's id, which names its temp file — see
        // `segment_tmp_path`. Two drivers for one key can only overlap at the
        // abandonment seam, and they must not share a file when they do.
        registration_id: u64,
    ) -> Result<Vec<u8>, HlsCacheError> {
        let path = self.segment_path_keyed(key);
        // V128: every exit from this function has to land in exactly one arm of
        // `pharos_segment_produced_total`, or the arms do not partition
        // production attempts and V91's `failed + coalesced_failed` sum is
        // short by whatever leaked. Four `?`s here recorded nothing — making
        // the cache directory, resolving the continuous-audio slice, and the
        // two that PUBLISH a finished encode — while every requester that
        // coalesced onto a driver dying at one of them WAS counted. So one
        // failed encode could produce N `coalesced_failed` and zero `failed`,
        // which reads as a blast radius with no blast.
        //
        // These are the paths a full, read-only or vanished cache volume takes,
        // which is the incident where the number is read.
        let counted = |step: &'static str, err: HlsCacheError| -> HlsCacheError {
            tracing::error!(
                media.id = media_id,
                seg = seg_index,
                step,
                reason = failure_reason(&err),
                error = %err,
                codec = codec_tag(opts.video, opts.audio_codec(), opts.container),
                path = %path.display(),
                "hls segment production failed outside the encode itself"
            );
            record_segment_failure(SegmentOutcome::Failed, failure_reason(&err), class);
            err
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| counted("create_dir", e.into()))?;
        }
        let tmp = segment_tmp_path(&path, registration_id);
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
            .await
            .map_err(|e| counted("resolve_audio", e))?;
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
            // Whoever this encode is FOR right now, which is not necessarily
            // who started it. Promotion re-ranks a job, and the retry is a
            // different job under a different id — joiners that already
            // promoted the first attempt do not promote again — so re-submitting
            // as `class` would silently hand the client back the speculative
            // tier it was rescued from, uncounted. Read fresh each attempt: a
            // client can arrive during attempt 0.
            //
            // Two label vocabularies below, and the split is deliberate. The
            // PRODUCTION outcomes (`pharos_segment_produced_total`,
            // `pharos_segment_cache_total`) stay labelled with the original
            // `class`: they count production attempts and requests, they
            // describe this driver, and the joiner already counted its own
            // request on the coalesced-hit path. The LATENCY histograms are
            // labelled `waiting_class` — who was actually waiting — because
            // they answer a different question, and a client-blocking encode
            // filed under `class="background"` is invisible to the interactive
            // p95 that phase 1's own validation reads.
            let submit_class = if promoted.load(std::sync::atomic::Ordering::Acquire) {
                JobClass::Interactive
            } else {
                class
            };
            // A submission raised to Interactive by somebody else's arrival is
            // made on THAT requester's behalf, and must not speak for this
            // driver's stream — see `JobHint::on_behalf_of_a_joiner`. When the
            // class did not move, this driver IS the requester and the hint is
            // its own.
            let submit_hint = if submit_class == class {
                hint
            } else {
                hint.on_behalf_of_a_joiner()
            };
            timing = match self
                .write_segment(
                    source,
                    &attempt_opts,
                    &tmp,
                    submit_class,
                    submit_hint,
                    &job_slot,
                )
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
        // Raised BEFORE the rename, not after: a flag set afterwards leaves the
        // window it exists to close. Setting it early can only cost a
        // cancellation that was about to become unsafe anyway — the encode is
        // finished by this line, so there is nothing left to reclaim either way.
        published.store(true, std::sync::atomic::Ordering::Release);
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| counted("publish_rename", e.into()))?;

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| counted("publish_read_back", e.into()))?;
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
        //
        // Labelled by WHO WAS WAITING, which for a promoted encode is not who
        // submitted it. A speculative driver that a client coalesced onto is a
        // client-blocking encode, and filing it under `class="background"`
        // understates interactive latency in exactly the case that matters:
        // Gate A measured 42% of interactive cache hits arriving coalesced, so
        // a large share of the slow client-visible encodes would sit outside
        // the interactive p95 that phase 1's validation condition ("a rising
        // allowance beside a rising p95 means the control law is wrong") is
        // read from. The scheduler's own `observe_margin` already treats a
        // promoted job as interactive; these two are meant to be read together
        // and must not disagree about the same job.
        let waiting_class = if class == JobClass::Interactive
            || promoted.load(std::sync::atomic::Ordering::Acquire)
        {
            JobClass::Interactive
        } else {
            class
        };
        if let Some(t) = timing.as_ref() {
            metrics::histogram!(
                "pharos_transcode_queue_wait_seconds",
                "class" => waiting_class.label(),
            )
            .record(t.queue_wait_ms as f64 / 1000.0);
            metrics::histogram!(
                "pharos_transcode_encode_seconds",
                "class" => waiting_class.label(),
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
                "class" => waiting_class.label(),
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
    #[allow(clippy::too_many_arguments)]
    async fn write_segment(
        &self,
        source: &Path,
        opts: &TranscodeOptions,
        out: &Path,
        class: JobClass,
        hint: JobHint,
        job_slot: &pharos_transcode::scheduler::JobSlot,
    ) -> Result<Option<pharos_transcode::scheduler::JobDone>, HlsCacheError> {
        let _ = source.to_str().ok_or(HlsCacheError::NonUtf8Path)?;
        // Scheduler path: the worker writes the segment file itself,
        // load-balanced across GPUs + CPU. We just await completion.
        if let Some(sched) = &self.scheduler {
            use pharos_transcode::scheduler::SinkRequest;
            let done = sched
                .submit_tracked(
                    source.to_path_buf(),
                    opts.clone(),
                    SinkRequest::FileDirect {
                        out_path: out.to_path_buf(),
                    },
                    class,
                    hint,
                    // Re-published on the retry attempt too: a retried encode is
                    // a NEW job, and promoting the one it replaced would leave
                    // the client waiting on the tier it was rescued from.
                    Some(job_slot.clone()),
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
                        StreamKey::NONE,
                        PlayheadSeed::Observes,
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
    /// again. A detached driver outlives the requester that started it — for as
    /// long as SOMEBODY is still waiting for the bytes, which is the whole
    /// content of the rule and what
    /// `an_encode_nobody_is_waiting_for_is_cancelled_rather_than_run_to_completion`
    /// pins from the other side.
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

    /// Block until `key`'s registration is present (or gone), or fail saying so.
    ///
    /// An observable, not a wall-clock premise — the same reasoning as
    /// `await_coalescers`. A sleep long enough on an idle box and too short on a
    /// loaded one produces a failure describing something that never happened.
    async fn await_registration(
        cache: &HlsSegmentCache,
        key: SegmentIdentity,
        present: bool,
        within: std::time::Duration,
        what: &str,
    ) {
        let deadline = std::time::Instant::now() + within;
        while cache.inflight.contains_key(&key) != present {
            assert!(std::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// The other half of the detached-driver rule, and the half phase 2a lost.
    ///
    /// `PrefetchRegistry` cancels a session's outstanding prefetch on an episode
    /// swap and on stop (PR #75), and it does so by aborting the spawned task.
    /// Once the driver was detached, that abort killed only the WAITER: the
    /// scheduler's `oneshot` lived on inside the driver with nothing holding a
    /// handle to it, so `reply.is_closed()` was false for every abandoned
    /// prefetch and `reap_abandoned` / `QueueOutcome::Abandoned` were near-dead
    /// for the case they were written for. An episode swap left roughly 6-14
    /// orphaned encodes for the previous episode draining onto the GPU while the
    /// new episode was starting — and V58's claim that "a seek or a track swap
    /// closes its `oneshot` and the abandonment sweep collects it" had become
    /// false.
    ///
    /// Asserted on the two things a viewer would feel: the registration goes
    /// well INSIDE the encode's own duration, so the work really stopped rather
    /// than finishing quietly, and no segment is published for it.
    #[test]
    fn an_encode_nobody_is_waiting_for_is_cancelled_rather_than_run_to_completion() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, _encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(600));
        let opts = slow_opts();
        let key = SegmentIdentity::new(61, 9, None, None, &opts);
        let path = cache.segment_path_keyed(key);
        let cache = Arc::new(cache);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let prefetch = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(61, 9, Path::new("/no/source"), &o, JobClass::Background)
                            .await
                    })
                };
                await_registration(
                    &cache,
                    key,
                    true,
                    std::time::Duration::from_secs(5),
                    "the prefetch never registered an encode, so there is nothing \
                     here to abandon",
                )
                .await;

                // The episode swap: exactly what `PrefetchRegistry` does.
                prefetch.abort();

                await_registration(
                    &cache,
                    key,
                    false,
                    std::time::Duration::from_millis(300),
                    "the abandoned encode was still registered 300 ms in, i.e. it \
                     is running to completion on a segment nobody will ever fetch",
                )
                .await;
                // Past the point the encode would have finished, had it run.
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            })
        });

        assert!(
            !path.exists(),
            "the abandoned encode published a segment, so it ran to completion"
        );
        let (labels, value) = produced_series(&snapshotter, "cancelled").expect(
            "an encode stopped because every requester went away must be counted \
             — it is the only signal that says how much speculative work an \
             episode swap abandons before it can start",
        );
        assert!(
            labels.contains(&"class=background".to_string()),
            "the abandoned driver's own class: {labels:?}"
        );
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one cancelled encode, got {value:?}"
        );
    }

    /// Block until the scheduler is actually RUNNING `n` jobs, or fail saying so.
    ///
    /// The observable that separates "queued" from "dispatched", which is the
    /// whole subject of
    /// `a_dispatched_encode_survives_its_last_requester_and_reaches_the_cache`.
    /// A sleep cannot stand in for it: too short and the job is still in the
    /// queue, where cancelling it is correct, so the test would pass while
    /// asserting nothing.
    async fn await_inflight(
        sched: &pharos_transcode::scheduler::TranscodeScheduler,
        n: usize,
        what: &str,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if sched.snapshot().await.map(|s| s.inflight) == Some(n) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// The other edge of the cancellation rule, and the one the inline stub
    /// cannot see.
    ///
    /// `an_encode_nobody_is_waiting_for_is_cancelled_rather_than_run_to_completion`
    /// runs on the inline transcoder, where dropping the driver genuinely stops
    /// the encode — so it is green either way and says nothing about this. On the
    /// SCHEDULER, dropping the driver stops nothing: `spawn_run_task` is detached
    /// and owns the worker plus the device permit, and every `reply.is_closed()`
    /// read is pre-dispatch. Cancelling there spends the encode and then throws
    /// the bytes away, and leaves the orphan writing to `{seg}.ts.tmp` and its
    /// `-progress` sidecar — both derived from the KEY — so the next requester
    /// for the same key (a refresh, an `hls.js` `fragLoadTimeout` retry, a
    /// swap-and-swap-back) starts a second worker on the same two files, and
    /// `short_of_frames(None, _)` fails open when the sidecar has been consumed
    /// by the wrong reader. A corrupt segment, cached, served with a 200.
    ///
    /// Asserted on the two consequences an operator would see: the segment is
    /// published (the work was not wasted) and the attempt is counted `ok`, never
    /// `cancelled` — one attempt, one arm, which is what V128 requires.
    #[test]
    fn a_dispatched_encode_survives_its_last_requester_and_reaches_the_cache() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, _encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let opts = slow_opts();
        let key = SegmentIdentity::new(62, 4, None, None, &opts);
        let path = cache.segment_path_keyed(key);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let sched = writes_each_segment(std::time::Duration::from_millis(400));
                let cache = Arc::new(cache.with_scheduler(sched.clone()));
                let prefetch = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(62, 4, Path::new("/no/source"), &o, JobClass::Background)
                            .await
                    })
                };
                await_inflight(
                    &sched,
                    1,
                    "the encode never reached a worker, so nothing here is about a \
                     DISPATCHED job",
                )
                .await;

                // The episode swap, arriving after the GPU has already been
                // spent on this segment.
                prefetch.abort();

                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !path.exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "a dispatched encode was abandoned: the worker held its permit \
                         and ran to completion anyway, and the bytes it produced were \
                         thrown away instead of cached — while its orphaned tmp file \
                         stayed open on the path the next requester will write to"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                // The file appearing is not the last thing the driver does. It
                // renames, reads the bytes back and only THEN records the
                // outcome, so leaving the runtime the instant the path exists
                // races the counter this test goes on to assert — and the local
                // recorder is torn down with the closure, so the increment
                // lands nowhere. Wait for the observable actually being
                // asserted, not for a proxy that precedes it.
                //
                // Its own deadline, not the publish loop's: reusing that one
                // would let a slow publish eat most of the 5 s budget and then
                // fail this wait almost immediately, blaming the counter for a
                // timeout the publish partition actually spent.
                let counter_deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(5);
                while produced_series(&snapshotter, "ok").is_none() {
                    assert!(
                        std::time::Instant::now() < counter_deadline,
                        "the segment was published but no production attempt was \
                         ever counted: pharos_segment_produced_total must partition \
                         attempts (V128), so a published segment with no arm is a \
                         hole in the partition"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
        });

        assert!(
            produced_series(&snapshotter, "cancelled").is_none(),
            "an encode that ran to completion and published a segment must not \
             also be counted as cancelled — the arms of \
             pharos_segment_produced_total partition production attempts (V128)"
        );
        let (labels, value) = produced_series(&snapshotter, "ok")
            .expect("the segment was published, so the attempt ended `ok`");
        assert!(
            labels.contains(&"class=background".to_string()),
            "the driver's own class: {labels:?}"
        );
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one produced segment, got {value:?}"
        );
    }

    /// A driver whose registration is not there any more must STOP, not spin.
    ///
    /// The wait loop re-takes its decision because a joiner may `subscribe`
    /// after the wake, reopening the channel. That is the only reason to loop,
    /// and it was inferred from a `remove_if` that MISSED — which is also what an
    /// absent entry, or a successor's entry, looks like. In that state
    /// `tx.closed()` is already resolved (our own receiver count is zero), so
    /// the loop has no await point in it: one poll, spinning forever, pegging a
    /// runtime worker and wedging the task.
    ///
    /// Unreachable in production today — the only removers are the id-qualified
    /// guard and the wait loop itself — so this pins the property rather than a
    /// bug, and pins it because the distance from "unreachable" to "reachable"
    /// is one unqualified `inflight.remove` written by somebody who did not know.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_driver_whose_registration_vanished_stops_instead_of_spinning() {
        let opts = slow_opts();
        let key = SegmentIdentity::new(70, 1, None, None, &opts);
        let job = pharos_transcode::scheduler::JobSlot::new();
        let published = std::sync::atomic::AtomicBool::new(false);

        for (what, seed) in [
            ("no entry at all", None),
            ("a successor's entry", Some(8u64)),
        ] {
            let map: DashMap<SegmentIdentity, InFlightSegment> = DashMap::new();
            let (tx, rx) = tokio::sync::watch::channel(None);
            // No receivers: the registry holds none, so this is "nobody is
            // waiting" — the state the wait loop wakes on.
            drop(rx);
            if let Some(other) = seed {
                map.insert(
                    key,
                    InFlightSegment {
                        id: other,
                        tx: Arc::new(tokio::sync::watch::channel(None).0),
                        driver_class: JobClass::Interactive,
                        job: pharos_transcode::scheduler::JobSlot::new().subscribe(),
                        promoted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    },
                );
            }
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                await_last_requester_gone(&tx, &map, key, 7, &job, &published),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "with {what} the driver span without yielding: an infinite loop \
                     inside one poll, which pegs a runtime worker and wedges this \
                     task for the life of the process"
                )
            });
        }
    }

    /// The belt on the abandonment seam: two drivers for one key must not share
    /// a temp file, nor the `-progress` sidecar derived from it.
    ///
    /// Ending the cancellable region at dispatch closes the ordinary case, but
    /// the flag is set inside the actor's dispatch step while the driver reads
    /// it outside — so a driver can read "not dispatched", deregister and drop
    /// its future in the instant the actor dispatches that job. The successor
    /// then shares `{seg}.ts.tmp` with a worker still writing it, and
    /// `short_of_frames` fails open when the sidecar has been consumed by the
    /// wrong reader: a corrupt segment, renamed into the cache and served with a
    /// 200 for as long as it stays there. Distinct names make that interleaving
    /// harmless rather than rare.
    #[test]
    fn two_registrations_for_one_key_never_write_the_same_file() {
        let path = Path::new("/cache/61/9.ts");
        let first = segment_tmp_path(path, 7);
        let second = segment_tmp_path(path, 8);
        assert_ne!(
            first, second,
            "an abandoned driver's worker and its successor would write the same \
             temp file, and whichever renamed first would publish the other's \
             half-written bytes"
        );
        assert_ne!(
            progress_sidecar_path(&first),
            progress_sidecar_path(&second),
            "the completeness gate reads a sidecar it CONSUMES, so sharing one \
             lets a successor judge its segment on somebody else's report — or on \
             none at all, which fails open"
        );
        assert_ne!(
            first, path,
            "the temp file must never be the published path: a reader would serve \
             a partial encode"
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
    ) -> Arc<tokio::sync::watch::Sender<Option<SharedSegment>>> {
        let (tx, rx) = tokio::sync::watch::channel(None);
        // Dropped at once: the registry holds no receiver, so every receiver
        // that exists belongs to a requester. That is what a real registration
        // does, and it is what makes `receiver_count()` mean "requesters" in
        // `await_coalescers`.
        drop(rx);
        let tx = Arc::new(tx);
        cache.inflight.insert(
            key,
            InFlightSegment {
                id: cache
                    .next_registration
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                tx: tx.clone(),
                driver_class: JobClass::Interactive,
                job: pharos_transcode::scheduler::JobSlot::new().subscribe(),
                promoted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        tx
    }

    /// End the seeded encode exactly as a real driver ends: publish the outcome,
    /// THEN drop the registration, THEN drop the sender. That order is the one
    /// the `InFlightGuard` enforces, and a test that used any other order would
    /// be testing a driver pharos does not have.
    fn finish_seeded_driver(
        cache: &HlsSegmentCache,
        key: SegmentIdentity,
        tx: Arc<tokio::sync::watch::Sender<Option<SharedSegment>>>,
        outcome: SharedSegment,
    ) {
        let _ = tx.send(Some(outcome));
        cache.inflight.remove(&key);
        drop(tx);
    }

    /// Abandon the seeded encode exactly as a DYING driver leaves it: the
    /// registration goes and the sender drops with nothing ever published.
    ///
    /// That state is not a synthetic one —
    /// `a_driver_dropped_before_its_first_poll_leaves_no_registration` proves a
    /// real runtime shutdown produces precisely it, guard first, sender with it.
    fn abandon_seeded_driver(
        cache: &HlsSegmentCache,
        key: SegmentIdentity,
        tx: Arc<tokio::sync::watch::Sender<Option<SharedSegment>>>,
    ) {
        cache.inflight.remove(&key);
        drop(tx);
    }

    /// Block until `n` receivers hold the seeded channel — i.e. until a
    /// requester has ACTUALLY coalesced onto it.
    ///
    /// An observable, not a wall-clock premise. A sleep that is long enough on
    /// an idle box and too short on a loaded one lets the requester miss the
    /// registration and drive its own encode, after which the test fails
    /// describing something that never happened. Deadlined, so a requester that
    /// never arrives fails here — naming that — instead of downstream.
    async fn await_coalescers(
        tx: &Arc<tokio::sync::watch::Sender<Option<SharedSegment>>>,
        n: usize,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while tx.receiver_count() < n {
            assert!(
                std::time::Instant::now() < deadline,
                "no requester coalesced onto the seeded encode ({} of {n} receivers)",
                tx.receiver_count()
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// The `pharos_segment_produced_total` series carrying `outcome=<want>`,
    /// as `(labels, value)`.
    fn produced_series(
        snapshotter: &metrics_util::debugging::Snapshotter,
        want: &str,
    ) -> Option<(Vec<String>, metrics_util::debugging::DebugValue)> {
        let want = format!("outcome={want}");
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_segment_produced_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                if !labels.contains(&want) {
                    return None;
                }
                Some((labels, v))
            })
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

    /// ...and how often that fall-through fires is a QUERY, not a debug line.
    ///
    /// The re-drive is the safety valve for a defect that reached a viewer as a
    /// 500 on a video segment (B134/V127). At `debug!` it is invisible at the
    /// deployment's log level, so neither its rate — is the valve carrying real
    /// load? — nor its disappearance, if a refactor stopped the fall-through
    /// firing, could be seen at all.
    #[test]
    fn the_rate_a_requester_declines_another_jobs_shed_is_countable() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(50));
        let opts = slow_opts();
        let key = SegmentIdentity::new(44, 8, None, None, &opts);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let tx = seed_inflight(&cache, key);
                let waiter = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(44, 8, Path::new("/no/source"), &o, JobClass::Interactive)
                            .await
                    })
                };
                await_coalescers(&tx, 1).await;
                finish_seeded_driver(&cache, key, tx, Err(Arc::new(HlsCacheError::SchedulerBusy)));
                waiter
                    .await
                    .unwrap()
                    .expect("the requester must drive its own encode");
            })
        });
        assert_eq!(
            encode_count(&encodes),
            1,
            "precondition: the requester declined the shed and drove its own"
        );

        let (labels, value) = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_segment_redrive_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                Some((labels, v))
            })
            .expect(
                "a requester declining another job's admission verdict must emit \
                 pharos_segment_redrive_total — a safety valve nobody can query \
                 is a safety valve nobody can tell has stopped working",
            );
        assert!(
            labels.contains(&"class=interactive".to_string()),
            "the re-driving requester's own class: {labels:?}"
        );
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one re-drive, got {value:?}"
        );
    }

    /// A scheduler whose one worker holds its permit for `hold`, so a job can
    /// be observed mid-flight.
    fn holds_each_job(
        hold: std::time::Duration,
        cpu_permits: usize,
        cfg: pharos_transcode::scheduler::SchedConfig,
    ) -> pharos_transcode::scheduler::TranscodeScheduler {
        use pharos_transcode::protocol::{JobSpec, WorkerId};
        use pharos_transcode::scheduler::{
            RunFuture, SpawnFuture, TranscodeScheduler, Worker, WorkerRunResult, WorkerSpawner,
        };

        struct Slow(std::time::Duration);
        impl WorkerSpawner for Slow {
            fn spawn(&self, id: WorkerId) -> SpawnFuture {
                let hold = self.0;
                Box::pin(async move { Ok(Box::new(SlowWorker { id, hold }) as Box<dyn Worker>) })
            }
        }
        struct SlowWorker {
            id: WorkerId,
            hold: std::time::Duration,
        }
        impl Worker for SlowWorker {
            fn id(&self) -> WorkerId {
                self.id
            }
            fn run<'a>(&'a mut self, _job: JobSpec) -> RunFuture<'a> {
                let hold = self.hold;
                Box::pin(async move {
                    tokio::time::sleep(hold).await;
                    WorkerRunResult::Done { out_bytes: 1 }
                })
            }
        }

        TranscodeScheduler::spawn(
            pharos_transcode::device::DeviceTable::from_probe(&[], cpu_permits),
            Arc::new(Slow(hold)),
            cfg,
        )
    }

    /// The wiring that makes deferring speculative work safe (V58).
    ///
    /// Coalescing shares the RESULT of an encode, which is always right. It must
    /// not share the DRIVER'S PRIORITY, which was decided about the driver
    /// (V127): a client that joins a prefetch and silently adopts its tier waits
    /// behind every other client's work, including work submitted after it
    /// started waiting. That is B108's harm arriving by a different route.
    ///
    /// Asserted through the scheduler's own snapshot, so it proves the job's
    /// class actually changed rather than that a message was sent.
    #[tokio::test]
    async fn an_interactive_requester_joining_a_prefetch_promotes_it() {
        let dir = TempDir::new().unwrap();
        let (cache, _) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let sched = holds_each_job(
            std::time::Duration::from_millis(600),
            4,
            pharos_transcode::scheduler::SchedConfig::default(),
        );
        let cache = Arc::new(cache.with_scheduler(sched.clone()));
        let opts = slow_opts();

        // Speculative warm-up starts the encode.
        let driver = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(9, 3, Path::new("/no/source"), &o, JobClass::Background)
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let before = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            before
                .devices
                .iter()
                .map(|d| d.inflight_background)
                .sum::<usize>(),
            1,
            "precondition: the prefetch is running as speculation"
        );

        // ...and then a client asks for the same segment and coalesces onto it.
        let joiner = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(9, 3, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;

        let after = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            after
                .devices
                .iter()
                .map(|d| d.inflight_background)
                .sum::<usize>(),
            0,
            "the client's arrival must have re-ranked the encode it is waiting on"
        );
        assert_eq!(
            after
                .devices
                .iter()
                .map(|d| d.inflight_interactive)
                .sum::<usize>(),
            1,
            "...as a client's own work"
        );
        // Exactly one encode: promotion re-ranks the job, it does not start a
        // second one.
        assert_eq!(after.inflight, 1, "promotion must not duplicate the encode");
        let _ = driver.await;
        let _ = joiner.await;
    }

    /// A scheduler whose worker writes a real segment file plus the
    /// `-progress` sidecar the completeness check reads — and reports the
    /// `short_at`-th job it sees short of video frames, so the driver retries
    /// that one under a new job id.
    ///
    /// `holds_each_job` cannot express this: it writes nothing, so
    /// `read_progress` finds no sidecar, `short_of_frames` returns `None`, and
    /// the retry path is unreachable.
    ///
    /// WHICH job comes back short is a parameter because a test may need to
    /// establish some scheduler state — a stream's playhead, say — with a job
    /// that simply succeeds before the one under observation arrives.
    fn retries_the_nth_encode(
        attempts: Arc<std::sync::atomic::AtomicU64>,
        short_at: u64,
    ) -> pharos_transcode::scheduler::TranscodeScheduler {
        use pharos_transcode::protocol::{JobSpec, OutputSink, WorkerId};
        use pharos_transcode::scheduler::{
            RunFuture, SpawnFuture, TranscodeScheduler, Worker, WorkerRunResult, WorkerSpawner,
        };

        struct Retrying(Arc<std::sync::atomic::AtomicU64>, u64);
        impl WorkerSpawner for Retrying {
            fn spawn(&self, id: WorkerId) -> SpawnFuture {
                let attempts = self.0.clone();
                let short_at = self.1;
                Box::pin(async move {
                    Ok(Box::new(RetryWorker {
                        id,
                        attempts,
                        short_at,
                    }) as Box<dyn Worker>)
                })
            }
        }
        struct RetryWorker {
            id: WorkerId,
            attempts: Arc<std::sync::atomic::AtomicU64>,
            short_at: u64,
        }
        impl Worker for RetryWorker {
            fn id(&self) -> WorkerId {
                self.id
            }
            fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a> {
                let attempts = self.attempts.clone();
                let short_at = self.short_at;
                Box::pin(async move {
                    let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let short = n == short_at;
                    // The attempt AFTER the short one is the one under
                    // observation, so it has to still be running when the
                    // snapshot is taken; the short one gets out of the way.
                    let hold = if short {
                        std::time::Duration::from_millis(250)
                    } else {
                        std::time::Duration::from_millis(600)
                    };
                    tokio::time::sleep(hold).await;
                    let OutputSink::FileDirect { path } = &job.sink else {
                        return WorkerRunResult::Died;
                    };
                    let _ = tokio::fs::write(path, vec![b'x'; 256]).await;
                    // `frame=0` is "no video frames at all" — ffmpeg exiting 0
                    // having produced nothing, the case the retry exists for.
                    let frames = if short { 0 } else { 150 };
                    let _ = tokio::fs::write(
                        pharos_transcode::progress_sidecar_path(path),
                        format!("frame={frames}\nout_time_us=600000000\n"),
                    )
                    .await;
                    WorkerRunResult::Done { out_bytes: 256 }
                })
            }
        }

        TranscodeScheduler::spawn(
            pharos_transcode::device::DeviceTable::from_probe(&[], 4),
            Arc::new(Retrying(attempts, short_at)),
            pharos_transcode::scheduler::SchedConfig::default(),
        )
    }

    /// A scheduler whose worker writes a REAL segment (plus a complete
    /// `-progress` sidecar) after `hold`.
    ///
    /// `holds_each_job` cannot be used for anything downstream of production:
    /// it writes no file, so `produce_segment` rejects the result at the
    /// minimum-size gate and returns before the timing histograms — which sit
    /// after the rename — are ever reached.
    fn writes_each_segment(
        hold: std::time::Duration,
    ) -> pharos_transcode::scheduler::TranscodeScheduler {
        use pharos_transcode::protocol::{JobSpec, OutputSink, WorkerId};
        use pharos_transcode::scheduler::{
            RunFuture, SpawnFuture, TranscodeScheduler, Worker, WorkerRunResult, WorkerSpawner,
        };

        struct Writing(std::time::Duration);
        impl WorkerSpawner for Writing {
            fn spawn(&self, id: WorkerId) -> SpawnFuture {
                let hold = self.0;
                Box::pin(async move { Ok(Box::new(WriteWorker { id, hold }) as Box<dyn Worker>) })
            }
        }
        struct WriteWorker {
            id: WorkerId,
            hold: std::time::Duration,
        }
        impl Worker for WriteWorker {
            fn id(&self) -> WorkerId {
                self.id
            }
            fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a> {
                let hold = self.hold;
                Box::pin(async move {
                    tokio::time::sleep(hold).await;
                    let OutputSink::FileDirect { path } = &job.sink else {
                        return WorkerRunResult::Died;
                    };
                    let _ = tokio::fs::write(path, vec![b'x'; 256]).await;
                    let _ = tokio::fs::write(
                        pharos_transcode::progress_sidecar_path(path),
                        "frame=150\nout_time_us=600000000\n",
                    )
                    .await;
                    WorkerRunResult::Done { out_bytes: 256 }
                })
            }
        }

        TranscodeScheduler::spawn(
            pharos_transcode::device::DeviceTable::from_probe(&[], 4),
            Arc::new(Writing(hold)),
            pharos_transcode::scheduler::SchedConfig::default(),
        )
    }

    /// An encode a client ended up blocked on is timed as INTERACTIVE, however
    /// it started.
    ///
    /// `pharos_transcode_encode_seconds{class}` is half of phase 1's own
    /// validation condition — "a rising allowance beside a rising p95 means the
    /// control law is wrong" — and that p95 cannot see a slow client-visible
    /// encode filed under `class="background"`. It is not a rare filing either:
    /// 42% of interactive cache hits on the deployment arrive coalesced onto an
    /// encode somebody else started. The scheduler's `observe_margin` already
    /// treats a promoted job as interactive, so labelling from the driver's
    /// registration class also left the two signals an operator reads together
    /// disagreeing about the same job.
    #[test]
    fn a_client_blocking_encode_is_timed_as_interactive_however_it_started() {
        use metrics_util::debugging::DebuggingRecorder;

        let dir = TempDir::new().unwrap();
        let (base, _) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let opts = slow_opts();

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                // Built inside the runtime: the scheduler actor is a spawned
                // task.
                let sched = writes_each_segment(std::time::Duration::from_millis(400));
                let cache = Arc::new(base.with_scheduler(sched));
                // A guess starts the encode...
                let driver = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(31, 5, Path::new("/no/source"), &o, JobClass::Background)
                            .await
                    })
                };
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                // ...and a client turns out to be waiting on it.
                let joiner = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(31, 5, Path::new("/no/source"), &o, JobClass::Interactive)
                            .await
                    })
                };
                driver.await.unwrap().expect("the encode itself succeeded");
                joiner.await.unwrap().expect("the joiner got the bytes");
            })
        });

        let classes: Vec<String> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, _, _, _)| ck.key().name() == "pharos_transcode_encode_seconds")
            .flat_map(|(ck, _, _, _)| {
                ck.key()
                    .labels()
                    .filter(|l| l.key() == "class")
                    .map(|l| l.value().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();

        assert_eq!(
            classes,
            vec!["interactive".to_string()],
            "an encode a client is blocked on must be timed as interactive, \
             whoever submitted it — otherwise the interactive p95 phase 1 is \
             validated against cannot see it"
        );
    }

    /// Publishing a finished encode into the cache is still PRODUCING it, so a
    /// failure there is counted like any other.
    ///
    /// The rename and the read-back were the one route out of `produce_segment`
    /// that recorded nothing. V91 makes `failed + coalesced_failed` the
    /// client-visible failure total and V128 makes
    /// `pharos_segment_produced_total`'s arms a partition of production
    /// attempts — and every COALESCER onto a driver that died here WAS counted,
    /// while the driver was not. The partition claim was simply false on this
    /// path, and it is the path a full or read-only cache volume takes.
    ///
    /// Forced by making the destination path a directory, so the rename cannot
    /// succeed while everything before it does.
    #[test]
    fn a_segment_that_cannot_be_published_is_counted_as_a_failure() {
        use metrics_util::debugging::DebugValue;
        use metrics_util::debugging::DebuggingRecorder;

        let dir = TempDir::new().unwrap();
        let (base, _) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let opts = slow_opts();
        let blocked = base.segment_path_keyed(SegmentIdentity::new(41, 2, None, None, &opts));

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let err = metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let sched = writes_each_segment(std::time::Duration::from_millis(400));
                let cache = Arc::new(base.with_scheduler(sched));
                let req = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(41, 2, Path::new("/no/source"), &o, JobClass::Interactive)
                            .await
                    })
                };
                // Occupy the destination AFTER the cache-hit lookup has already
                // missed it, so the encode runs in full and it is the PUBLISH
                // that fails — a directory cannot be the target of a file
                // rename. Placing it up front would instead be answered by the
                // fast-hit read, which never reaches production at all.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::fs::create_dir_all(&blocked).unwrap();
                req.await.unwrap()
            })
        })
        .expect_err("the rename cannot succeed onto a directory");
        assert!(
            matches!(err, HlsCacheError::Io(_)),
            "the caller must be told what actually went wrong: {err:?}"
        );

        let (labels, value) = produced_series(&snapshotter, "failed").expect(
            "an encode that could not be published must be counted — every \
             requester that coalesced onto it already is, so leaving the driver \
             out makes the produced-total partition false exactly where a full \
             cache volume shows up",
        );
        for want in ["outcome=failed", "reason=io", "class=interactive"] {
            assert!(
                labels.contains(&want.to_string()),
                "missing label {want}; got {labels:?}"
            );
        }
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one publish failure, got {value:?}"
        );
    }

    /// The promotion has to survive the retry, because the retry is a different
    /// JOB.
    ///
    /// A segment that comes back short of video frames is re-submitted under a
    /// new id with a deeper decode preroll. Promotion re-ranks one job, and a
    /// joiner that already promoted the first attempt does not promote again —
    /// so re-submitting as the driver's original class hands the client straight
    /// back to the speculative tier it was rescued from, silently and
    /// uncounted. The measured coalescing rate on the deployment makes that a
    /// routine path, not a corner: 11 of 26 interactive cache hits arrived by
    /// joining an in-flight speculative encode.
    ///
    /// Asserted through the scheduler's own snapshot DURING the second attempt,
    /// so it proves the class the retry was submitted at rather than that a
    /// flag was set.
    #[tokio::test]
    async fn a_retried_encode_keeps_the_tier_a_client_promoted_it_to() {
        let dir = TempDir::new().unwrap();
        let (cache, _) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sched = retries_the_nth_encode(attempts.clone(), 0);
        let cache = Arc::new(cache.with_scheduler(sched.clone()));
        // Video, so `frame=0` is a shortfall rather than a legitimate
        // audio-only segment.
        let opts = SegmentOpts {
            video: Some(SegmentVideo::H264),
            ..slow_opts()
        };

        let driver = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(11, 4, Path::new("/no/source"), &o, JobClass::Background)
                    .await
            })
        };
        // A client joins the first attempt and promotes it.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let joiner = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(11, 4, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };

        // The first attempt comes back short at ~250 ms and the driver
        // re-submits; this lands inside the second attempt.
        tokio::time::sleep(std::time::Duration::from_millis(320)).await;
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "precondition: the completeness check must have forced a retry"
        );
        let during = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            during
                .devices
                .iter()
                .map(|d| d.inflight_background)
                .sum::<usize>(),
            0,
            "the retry must not drop the client back to the speculative tier"
        );
        assert_eq!(
            during
                .devices
                .iter()
                .map(|d| d.inflight_interactive)
                .sum::<usize>(),
            1,
            "...it is still the client's own work"
        );

        let bytes = driver.await.unwrap().expect("the retry must succeed");
        assert_eq!(bytes.len(), 256);
        let joined = joiner
            .await
            .unwrap()
            .expect("the joiner gets the bytes too");
        assert_eq!(joined.len(), 256);
    }

    /// ...and the promoted re-submission must not drag the DRIVER'S viewer's
    /// playhead forward with it.
    ///
    /// The retry is Interactive because somebody JOINED, and `note_playhead`
    /// moves a stream's playhead on every interactive submission — but the
    /// joiner is on a stream the segment cache cannot name, while the stream
    /// the hint does name belongs to the speculative driver. Its viewer never
    /// asked for this segment.
    ///
    /// The harm is concrete: viewer A standing at segment 100 prefetches 106,
    /// anybody joins that guess, the retry declares A to be at 106 — and A's
    /// own queued prefetch for 101–105 is instantly stale, reaped as such, so A
    /// takes five cold misses before its next request drags the playhead back.
    /// `promote_job` refuses to move the playhead for exactly this reason; the
    /// re-submission is the same act arriving by another route.
    #[tokio::test]
    async fn a_promoted_retry_does_not_move_the_drivers_playhead() {
        let dir = TempDir::new().unwrap();
        let (cache, _) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Job 0 succeeds — it is only here to put a reading on the viewer's
        // stream. Job 1, the speculative driver, is the one that comes back
        // short and retries.
        let sched = retries_the_nth_encode(attempts.clone(), 1);
        let cache = Arc::new(cache.with_scheduler(sched.clone()));
        let opts = SegmentOpts {
            video: Some(SegmentVideo::H264),
            ..slow_opts()
        };
        let viewer = StreamKey::of("viewer-a");

        // Viewer A asks for its own segment 100: an interactive submission, so
        // the scheduler now knows where A stands.
        cache
            .segment_bytes_keyed(
                50,
                100,
                None,
                None,
                Path::new("/no/source"),
                &opts,
                JobClass::Interactive,
                viewer,
                PlayheadSeed::Observes,
            )
            .await
            .expect("the viewer's own segment");
        let before = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            before.playheads.get(&viewer).copied(),
            Some(100),
            "precondition: the viewer's position must be known"
        );

        // A's prefetch runs six segments ahead of that...
        let driver = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes_keyed(
                    50,
                    106,
                    None,
                    None,
                    Path::new("/no/source"),
                    &o,
                    JobClass::Background,
                    viewer,
                    PlayheadSeed::Observes,
                )
                .await
            })
        };
        // ...and somebody else turns out to want that segment now, promoting it.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let joiner = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes_keyed(
                    50,
                    106,
                    None,
                    None,
                    Path::new("/no/source"),
                    &o,
                    JobClass::Interactive,
                    StreamKey::of("viewer-b"),
                    PlayheadSeed::Observes,
                )
                .await
            })
        };
        driver.await.unwrap().expect("the retry must succeed");
        joiner
            .await
            .unwrap()
            .expect("the joiner gets the bytes too");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "precondition: the priming job, the short attempt and its retry"
        );

        let after = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            after.playheads.get(&viewer).copied(),
            Some(100),
            "a submission made on a JOINER's behalf must not declare the \
             driver's viewer to have reached the segment being guessed at — \
             everything that viewer has queued between the two becomes stale"
        );
    }

    /// Put a segment on disk for `seg` so the next request for it is a FAST
    /// cache hit — what a viewer sees once prefetch is a segment or two ahead.
    async fn already_cached(cache: &HlsSegmentCache, media: u64, seg: u32, opts: &SegmentOpts) {
        let path = cache.segment_path_keyed(SegmentIdentity::new(media, seg, None, None, opts));
        tokio::fs::create_dir_all(path.parent().expect("a segment path has a parent"))
            .await
            .expect("cache dir");
        tokio::fs::write(&path, vec![b'x'; 256])
            .await
            .expect("cached segment");
    }

    /// T109 — a viewer being served WELL must not go dark to the scheduler.
    ///
    /// The reading was miss-driven: `note_playhead` fired on an interactive
    /// `Submit`, and a `Submit` only happens when the cache could not answer. So
    /// the better prefetch worked, the staler the number got — a viewer at 104
    /// whose reading still said 100 had every one of its guesses ranked four
    /// segments further out than they were, and its next genuine miss queued
    /// behind another stream's shallower work.
    ///
    /// The miss first is the control: it proves this harness really does reach
    /// the scheduler, so the hits that follow are asserted against a reading
    /// that is demonstrably live rather than against an absent one.
    #[tokio::test]
    async fn a_warm_viewers_playhead_advances_on_cache_hits() {
        let dir = TempDir::new().unwrap();
        let (cache, _) = slow_test_cache(&dir, std::time::Duration::from_millis(5));
        let sched = writes_each_segment(std::time::Duration::ZERO);
        let cache = cache.with_scheduler(sched.clone());
        let opts = slow_opts();
        let viewer = StreamKey::of("warm-buffer-viewer");

        cache
            .segment_bytes_keyed(
                80,
                100,
                None,
                None,
                Path::new("/no/source"),
                &opts,
                JobClass::Interactive,
                viewer,
                PlayheadSeed::Observes,
            )
            .await
            .expect("the cold segment");
        let snap = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.playheads.get(&viewer).copied(),
            Some(100),
            "control: a MISS still moves the reading, so this harness reaches \
             the scheduler at all"
        );

        // ...and from here prefetch is far enough ahead that the viewer never
        // misses again.
        for seg in 101..=104u32 {
            already_cached(&cache, 80, seg, &opts).await;
            let bytes = cache
                .segment_bytes_keyed(
                    80,
                    seg,
                    None,
                    None,
                    Path::new("/no/source"),
                    &opts,
                    JobClass::Interactive,
                    viewer,
                    PlayheadSeed::Observes,
                )
                .await
                .expect("a warm hit");
            assert_eq!(bytes.len(), 256, "segment {seg} must be the CACHED bytes");
        }

        let snap = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.playheads.get(&viewer).copied(),
            Some(104),
            "a segment served from cache is the same evidence about where this \
             viewer stands as one that had to be encoded — a reading only a miss \
             can move goes stale exactly when playback is going well"
        );
    }

    /// ...and a hit that lands BEHIND the reading must not undo it.
    ///
    /// A client's parallel fetches around a seek complete out of order, and a
    /// hit says when the bytes were read rather than when they were asked for.
    /// A reading dragged back to 120 by a late hit ranks every guess on that
    /// stream twenty segments further out than it is — the exact error T109
    /// exists to remove, reintroduced from the other side.
    ///
    /// The MISS is what may move a reading in either direction: that is how a
    /// backward seek is expressed, and it is what keeps the hit rule from being
    /// a one-way ratchet.
    ///
    /// BOTH edges are asserted here on purpose, and the second is what makes the
    /// first mean anything. "The reading did not move backwards" is also what an
    /// implementation that never reports a hit at all produces — it is the
    /// behaviour this task exists to remove — so a test asserting only that
    /// would be green today and green forever. The forward hit that follows
    /// pins, in this same test, that hits DO reach the scheduler from here: the
    /// pair is passed only by an implementation that reports every hit and
    /// applies the backward one.
    #[tokio::test]
    async fn a_late_hit_behind_the_viewer_does_not_drag_the_playhead_back() {
        let dir = TempDir::new().unwrap();
        let (cache, _) = slow_test_cache(&dir, std::time::Duration::from_millis(5));
        let sched = writes_each_segment(std::time::Duration::ZERO);
        let cache = cache.with_scheduler(sched.clone());
        let opts = slow_opts();
        let viewer = StreamKey::of("seeking-viewer");

        cache
            .segment_bytes_keyed(
                81,
                140,
                None,
                None,
                Path::new("/no/source"),
                &opts,
                JobClass::Interactive,
                viewer,
                PlayheadSeed::Observes,
            )
            .await
            .expect("the segment the viewer jumped to");

        // Segment 120's bytes — asked for before the jump — arrive now.
        already_cached(&cache, 81, 120, &opts).await;
        cache
            .segment_bytes_keyed(
                81,
                120,
                None,
                None,
                Path::new("/no/source"),
                &opts,
                JobClass::Interactive,
                viewer,
                PlayheadSeed::Observes,
            )
            .await
            .expect("the late hit");

        let snap = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.playheads.get(&viewer).copied(),
            Some(140),
            "a hit is evidence the viewer REACHED that segment, never evidence \
             they went back to it: going back produces a request, and a request \
             is the thing allowed to move a reading either way"
        );

        // ...and hits are not simply going nowhere from here: the very next one
        // ahead of the reading moves it.
        already_cached(&cache, 81, 141, &opts).await;
        cache
            .segment_bytes_keyed(
                81,
                141,
                None,
                None,
                Path::new("/no/source"),
                &opts,
                JobClass::Interactive,
                viewer,
                PlayheadSeed::Observes,
            )
            .await
            .expect("the next segment, already warm");

        let snap = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.playheads.get(&viewer).copied(),
            Some(141),
            "the assertion above must be the RULE working, not the hit path \
             being silent: an implementation that reports no hit at all also \
             leaves the reading at 140"
        );
    }

    /// The coalesced half of T109, and the seam
    /// `a_promoted_retry_does_not_move_the_drivers_playhead` guards from the
    /// other side.
    ///
    /// A joiner served off somebody else's encode took a hit
    /// (`hit_path="coalesced"`), so it too reached the scheduler with nothing to
    /// say about where its own viewer stands. Reporting it has to move the
    /// JOINER'S reading and only the joiner's: the driver's viewer never asked
    /// for this segment — it is six ahead of them, a guess — and declaring them
    /// to be standing on it makes everything they have queued in between
    /// instantly stale.
    #[tokio::test]
    async fn a_coalesced_hit_moves_the_joiners_playhead_and_not_the_drivers() {
        let dir = TempDir::new().unwrap();
        let (cache, _) = slow_test_cache(&dir, std::time::Duration::from_millis(5));
        let sched = writes_each_segment(std::time::Duration::from_millis(150));
        let cache = Arc::new(cache.with_scheduler(sched.clone()));
        let opts = slow_opts();
        let driver_viewer = StreamKey::of("coalesce-driver-viewer");
        let joiner_viewer = StreamKey::of("coalesce-joiner-viewer");

        // Where the driver's viewer actually stands.
        cache
            .segment_bytes_keyed(
                82,
                100,
                None,
                None,
                Path::new("/no/source"),
                &opts,
                JobClass::Interactive,
                driver_viewer,
                PlayheadSeed::Observes,
            )
            .await
            .expect("the driver's own segment");

        // Its prefetch runs six ahead...
        let driver = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes_keyed(
                    82,
                    106,
                    None,
                    None,
                    Path::new("/no/source"),
                    &o,
                    JobClass::Background,
                    driver_viewer,
                    PlayheadSeed::Observes,
                )
                .await
            })
        };
        // ...and a different viewer turns out to want that segment now, and is
        // served by joining it rather than by an encode of its own.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let joiner = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes_keyed(
                    82,
                    106,
                    None,
                    None,
                    Path::new("/no/source"),
                    &o,
                    JobClass::Interactive,
                    joiner_viewer,
                    PlayheadSeed::Observes,
                )
                .await
            })
        };
        driver.await.unwrap().expect("the driver's bytes");
        joiner.await.unwrap().expect("the joiner's bytes");

        let snap = sched.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.playheads.get(&joiner_viewer).copied(),
            Some(106),
            "a coalesced hit is still a hit: the viewer it served has reached \
             that segment"
        );
        assert_eq!(
            snap.playheads.get(&driver_viewer).copied(),
            Some(100),
            "and it must move the JOINER'S reading only — the driver's viewer \
             never asked for this segment, and declaring them to be standing on \
             it makes everything they have queued in between stale"
        );
    }

    /// The simplest thing the scheduler will place on the CPU: no video
    /// judgement to make, nothing to resolve. It never encodes anything — it
    /// exists only to hold the permit.
    fn blocker_opts() -> pharos_transcode::TranscodeOptions {
        slow_opts()
            .resolve_with(|_| -> Result<_, std::convert::Infallible> {
                unreachable!("Separate audio never asks for a slice")
            })
            .expect("infallible")
            .to_transcode_options()
    }

    /// A scheduler that sheds every speculative job: one permit, already held
    /// by a client's encode, and no room to queue behind it.
    ///
    /// Since V58 a refused `Background` submission WAITS for a permit rather
    /// than dying, so "shed" is now what happens when the queue is full — which
    /// is what `pending_cap: 0` makes of every refusal. The device is genuinely
    /// occupied rather than merely reserved, because the reserve is clamped to
    /// the pool's capacity minus one and an idle pool therefore always has room
    /// for speculation; a helper that relied on the old "reserve >= capacity"
    /// trick would silently start ADMITTING the job it exists to refuse.
    ///
    /// Returns once the permit is genuinely taken, so a `Background` submission
    /// after this point is refused without ever reaching a worker.
    async fn always_sheds_background() -> pharos_transcode::scheduler::TranscodeScheduler {
        use pharos_transcode::scheduler::{
            JobClass as JC, JobHint as JH, SchedConfig, SinkRequest,
        };

        let sched = holds_each_job(
            std::time::Duration::from_secs(30),
            1,
            SchedConfig {
                pending_cap: 0,
                ..SchedConfig::default()
            },
        );
        let blocker = sched.clone();
        tokio::spawn(async move {
            blocker
                .submit(
                    PathBuf::from("/no/source"),
                    blocker_opts(),
                    SinkRequest::FileDirect {
                        out_path: PathBuf::from("/dev/null"),
                    },
                    JC::Interactive,
                    JH::default(),
                )
                .await
        });
        for _ in 0..200 {
            let snap = sched.snapshot().await.expect("snapshot");
            if snap.devices.iter().map(|d| d.in_use).sum::<usize>() > 0 {
                return sched;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the blocker never took the only permit");
    }

    /// A DRIVING requester keeps its own shed, and SUBMITS ONCE. Shedding work
    /// you submitted yourself is the intended behaviour (B108/V58), and the
    /// fall-through above must not quietly become a retry loop that defeats it:
    /// the re-drive is "do not adopt somebody else's decision", not "ask again
    /// until admitted".
    ///
    /// The submission count is what makes this a guard rail rather than a
    /// description. Delete `!driving` from the fall-through predicate and the
    /// driving requester falls through, re-registers, submits a SECOND time, is
    /// shed again — and still returns `SchedulerBusy` with zero encodes and no
    /// leaked registration, so an end-state-only test passes through the
    /// regression it exists to catch. The shed counter cannot: each submission
    /// records one, so a second submission is `Counter(2)`.
    #[test]
    fn a_driving_requester_keeps_its_own_shed() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(10));
        let opts = slow_opts();

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (cache, err) = metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                // The scheduler is built inside the runtime it will live on.
                let cache = cache.with_scheduler(always_sheds_background().await);
                // Nothing is in flight, so this request drives its own encode —
                // and its own encode is shed.
                let err = cache
                    .segment_bytes(5, 1, Path::new("/no/source"), &opts, JobClass::Background)
                    .await
                    .expect_err("a speculative job the scheduler declined must report the shed");
                (cache, err)
            })
        });
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

        let (_, value) = produced_series(&snapshotter, "shed")
            .expect("a shed submission must be counted: pharos_segment_produced_total{shed}");
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "the driving requester submitted more than once — the fall-through has become \
             a retry loop, which is exactly what B108/V58 forbids: {value:?}"
        );
        assert!(
            produced_series(&snapshotter, "coalesced_shed").is_none(),
            "a requester's own shed must not be booked as an inherited one"
        );
    }

    /// V91 symmetry, failure half. One failed encode is returned to N waiters
    /// while `pharos_segment_produced_total` increments ONCE, so the counter a
    /// segment-failure alert reads undercounts client-visible failures by the
    /// coalescing factor and nothing says how many requests one bad encode took
    /// down. `outcome="coalesced_failed"` is that missing count.
    #[test]
    fn a_request_failed_by_the_encode_it_joined_is_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, _encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(50));
        let opts = slow_opts();
        let key = SegmentIdentity::new(11, 5, None, None, &opts);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let tx = seed_inflight(&cache, key);
                let waiter = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(11, 5, Path::new("/no/source"), &o, JobClass::Interactive)
                            .await
                    })
                };
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                finish_seeded_driver(
                    &cache,
                    key,
                    tx,
                    Err(Arc::new(HlsCacheError::Transcode("ffmpeg exploded".into()))),
                );
                let err = waiter
                    .await
                    .unwrap()
                    .expect_err("the joined encode failed, so this request must fail");
                assert!(
                    matches!(&err, HlsCacheError::Transcode(m) if m.contains("ffmpeg exploded")),
                    "the driver's error must reach the waiter intact: {err:?}"
                );
            })
        });

        let (labels, value) = produced_series(&snapshotter, "coalesced_failed").expect(
            "a request failed by the encode it coalesced onto must emit \
             pharos_segment_produced_total{outcome=coalesced_failed} — without it one \
             failed encode counts once no matter how many clients it took down",
        );
        for want in [
            "outcome=coalesced_failed",
            "reason=transcode",
            "class=interactive",
        ] {
            assert!(
                labels.contains(&want.to_string()),
                "missing label {want}; got {labels:?}"
            );
        }
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one coalesced failure, got {value:?}"
        );
    }

    /// A client-visible LOAD-SHED is not an encode FAILURE, and the counter must
    /// not say it is.
    ///
    /// V91 tells an operator that `failed + coalesced_failed` is the
    /// client-visible failure total and that their ratio is the blast radius of
    /// one bad encode. `failure_reason(SchedulerBusy)` is `"scheduler_busy"`, so
    /// an inherited shed booked as `coalesced_failed` puts deliberate
    /// load-shedding inside that sum — and under a prefetch storm, the B108
    /// condition where Background work is shed BY DESIGN, it comes to dominate
    /// it. Following V91's own arithmetic then pages somebody for admission
    /// control working: the same shed/failure conflation the coalescing
    /// fall-through exists to keep off the response path, one signal layer down.
    ///
    /// Reaching a NON-driving shed takes two in-flight encodes, because the
    /// fall-through spends attempt 1 declining the first: this requester joins
    /// one shed encode, declines it, and finds a second already registered, so
    /// its second attempt is a join rather than a drive. `encode_count` proves
    /// that — a requester that drove its own would have run the stub.
    #[test]
    fn an_inherited_shed_is_counted_as_a_shed_not_a_failure() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(50));
        let opts = slow_opts();
        let key = SegmentIdentity::new(12, 6, None, None, &opts);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let first = seed_inflight(&cache, key);
                let waiter = {
                    let c = cache.clone();
                    let o = opts.clone();
                    tokio::spawn(async move {
                        c.segment_bytes(12, 6, Path::new("/no/source"), &o, JobClass::Interactive)
                            .await
                    })
                };
                await_coalescers(&first, 1).await;

                // A SECOND encode registers before the first publishes, so the
                // fall-through re-registers onto a live driver instead of
                // becoming one. Ordered this way deliberately: seeding after the
                // publish would race the woken requester.
                let second = seed_inflight(&cache, key);
                let _ = first.send(Some(Err(Arc::new(HlsCacheError::SchedulerBusy))));
                drop(first);

                await_coalescers(&second, 1).await;
                finish_seeded_driver(
                    &cache,
                    key,
                    second,
                    Err(Arc::new(HlsCacheError::SchedulerBusy)),
                );

                let err = waiter
                    .await
                    .unwrap()
                    .expect_err("both encodes it joined were shed, so this request must fail");
                assert!(
                    matches!(err, HlsCacheError::SchedulerBusy),
                    "the shed must reach the caller as a shed: {err:?}"
                );
            })
        });
        assert_eq!(
            encode_count(&encodes),
            0,
            "the requester drove its own encode, so this is no longer an INHERITED shed"
        );

        let (labels, value) = produced_series(&snapshotter, "coalesced_shed").expect(
            "a request shed by another job's admission decision must emit \
             pharos_segment_produced_total{outcome=coalesced_shed} — booking it as \
             coalesced_failed puts deliberate load-shedding inside the failure sum V91 \
             tells operators to alert on",
        );
        for want in [
            "outcome=coalesced_shed",
            "reason=scheduler_busy",
            "class=interactive",
        ] {
            assert!(
                labels.contains(&want.to_string()),
                "missing label {want}; got {labels:?}"
            );
        }
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one inherited shed, got {value:?}"
        );
        assert!(
            produced_series(&snapshotter, "coalesced_failed").is_none(),
            "a shed was counted as an encode failure; `failed + coalesced_failed` is the \
             sum V91 calls the client-visible failure total, and load-shedding is not a \
             failure of anything"
        );
    }

    /// A requester whose OWN driver dies before it produces anything is counted
    /// nowhere unless the requester counts it.
    ///
    /// `produce_segment` never ran, so no `failed` was recorded, and the
    /// coalesced counter used to be gated on `!driving` — so the one request
    /// this takes down disappeared from every counter. Shutdown-only, but it is
    /// the rich-on-one-arm asymmetry V91 exists to forbid, and the arm it is
    /// missing from is the failure arm.
    ///
    /// The driver is put on its OWN runtime — entered for the duration of the
    /// request, which is all `tokio::spawn` consults — and that runtime is
    /// ALREADY DEAD when the request is made. `Handle::spawn` on a shut-down
    /// runtime constructs the task and drops it without ever polling it, which
    /// is exactly the production shape (the task is dropped, its sender with
    /// it, nothing published) and needs no hook in production code.
    ///
    /// Killing the runtime from a second thread once the registration appeared
    /// in `inflight` looked more faithful and was a race: the registration is
    /// made on the REQUESTER's thread, before `tokio::spawn`, so the driver
    /// task could be polled and reach a `tokio::fs` call before the shutdown
    /// landed. It then published a real `Io("background task failed")` instead
    /// of dying silently, and the test failed on the error text — 5 runs in 25,
    /// load-dependent, and indistinguishable from a regression in the code it
    /// guards. Ordering the shutdown BEFORE the request removes the window
    /// rather than narrowing it: there is no interleaving left to lose.
    ///
    /// One constraint that binds future edits rather than this one. The dead
    /// runtime is ENTERED for the whole request, so anything the REQUESTER path
    /// does that needs a driver — a `tokio::time` timer above all, but any
    /// reactor registration — resolves against it and PANICS ("A Tokio 1.x
    /// context was found, but it is being shutdown") instead of failing an
    /// assertion. Today that path only awaits a `oneshot`, which needs no
    /// driver. Give the wait a timeout, or any deadline, and this test stops
    /// reporting on the code it guards and starts reporting on its own
    /// scaffolding.
    #[test]
    fn a_requester_whose_driver_dies_before_producing_counts_itself() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let dir = TempDir::new().unwrap();
        let (cache, _encodes) = slow_test_cache(&dir, std::time::Duration::from_secs(5));
        let opts = slow_opts();

        let driver_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let driver_handle = driver_rt.handle().clone();
        let requester_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // The driver's runtime dies here, before anything is asked of it. The
        // handle outlives it and still answers `enter()`, so the request below
        // still spawns its driver ONTO the dead runtime — it just never runs.
        driver_rt.shutdown_timeout(std::time::Duration::ZERO);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let err = metrics::with_local_recorder(&recorder, || {
            requester_rt.block_on(async {
                // Everything this request spawns lands on the doomed runtime;
                // the request itself is driven by this thread and outlives it.
                let _entered = driver_handle.enter();
                cache
                    .segment_bytes(13, 7, Path::new("/no/source"), &opts, JobClass::Interactive)
                    .await
            })
        })
        .expect_err("a driver that died without producing must fail the request that spawned it");

        assert!(
            matches!(&err, HlsCacheError::Transcode(m)
                if m.contains("driver stopped without publishing")),
            "the requester must be told WHY it was woken: {err:?}"
        );

        let (labels, value) = produced_series(&snapshotter, "coalesced_failed").expect(
            "a request whose driver died before producing anything must still be counted — \
             `produce_segment` never ran, so nothing else counted it, and gating the \
             coalesced counter on !driving dropped it from every series",
        );
        for want in [
            "outcome=coalesced_failed",
            "reason=transcode",
            "class=interactive",
        ] {
            assert!(
                labels.contains(&want.to_string()),
                "missing label {want}; got {labels:?}"
            );
        }
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "expected exactly one uncounted-driver failure, got {value:?}"
        );
    }

    /// The whole accounting rule, stated once. Each arm is a different reason a
    /// request can end without bytes, and merging any two of them is a wrong
    /// number in a dashboard rather than a compile error.
    #[test]
    fn a_failed_shared_wait_is_counted_exactly_once() {
        let boom = HlsCacheError::Transcode("ffmpeg exploded".into());
        let shed = HlsCacheError::SchedulerBusy;
        let gone = shared_copy(&SegmentWait::DriverGone.into_outcome().unwrap_err());

        // Published to the requester that produced it: `produce_segment` already
        // recorded `failed` / `shed`, so a second count is a double count.
        assert_eq!(shared_failure_outcome(&boom, true, false), None);
        assert_eq!(shared_failure_outcome(&shed, true, false), None);

        // The driver went away before `produce_segment` ran: counted nowhere
        // else, whether or not this request is the one that spawned it.
        assert_eq!(
            shared_failure_outcome(&gone, true, true),
            Some(SegmentOutcome::CoalescedFailed)
        );
        assert_eq!(
            shared_failure_outcome(&gone, false, true),
            Some(SegmentOutcome::CoalescedFailed)
        );

        // Somebody else's outcome: a fault counts as a failure, a load-shed
        // decision counts as a shed and stays out of the failure sum.
        assert_eq!(
            shared_failure_outcome(&boom, false, false),
            Some(SegmentOutcome::CoalescedFailed)
        );
        assert_eq!(
            shared_failure_outcome(&shed, false, false),
            Some(SegmentOutcome::CoalescedShed)
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
            let Registration { driving, .. } = cache.register_or_join(
                key,
                Path::new("/no/source"),
                &opts,
                3,
                0,
                JobClass::Interactive,
                JobHint::default(),
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

    /// Correctness requirement 2 END TO END, not as a `watch` property: a driver
    /// that dies WITHOUT publishing must fail its WAITERS — the requesters that
    /// merely joined it — with the named error rather than park them on a
    /// channel nobody can send to.
    ///
    /// The abandonment is the seeded one, and the reason is that the waiter's
    /// join is only observable on the seeded channel: it advances the sender's
    /// receiver count. The version of this test that killed a real runtime had
    /// to GUESS the join with a 300 ms sleep, and on a loaded box the guess
    /// fails in the most misleading way available — the driver's guard has
    /// already deregistered, so the waiter drives its OWN encode, races the
    /// test's own timeout and reports "the requester hung on a channel nobody
    /// can publish to" about a requester that was busy encoding. Nothing is
    /// given up: `abandon_seeded_driver` leaves exactly the state a killed
    /// runtime leaves, and that a killed runtime leaves it is proved next door
    /// by `a_driver_dropped_before_its_first_poll_leaves_no_registration` and by
    /// `a_requester_whose_driver_dies_before_producing_counts_itself`, which
    /// shuts a real runtime down under a real driver.
    #[tokio::test]
    async fn a_requester_is_told_when_its_driver_dies_without_publishing() {
        let dir = TempDir::new().unwrap();
        let (cache, encodes) = slow_test_cache(&dir, std::time::Duration::from_millis(50));
        let opts = slow_opts();
        let key = SegmentIdentity::new(7, 1, None, None, &opts);

        let tx = seed_inflight(&cache, key);
        let waiter = {
            let c = cache.clone();
            let o = opts.clone();
            tokio::spawn(async move {
                c.segment_bytes(7, 1, Path::new("/no/source"), &o, JobClass::Interactive)
                    .await
            })
        };
        // The join itself, not an interval in which it probably happened.
        await_coalescers(&tx, 1).await;

        // The driver goes away mid-encode: registration gone, sender dropped,
        // nothing ever published.
        abandon_seeded_driver(&cache, key, tx);

        let err = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("the waiter hung on a channel nobody can publish to")
            .unwrap()
            .expect_err("a driver that died without publishing must fail its waiters");
        assert!(
            matches!(&err, HlsCacheError::Transcode(m)
                if m.contains("driver stopped without publishing")),
            "the waiter must be told WHY it was woken: {err:?}"
        );
        assert_eq!(
            encode_count(&encodes),
            0,
            "the waiter drove its own encode instead of joining the doomed one, so this \
             is no longer a test about a dead driver"
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
            SegmentOutcome::CoalescedFailed,
            SegmentOutcome::CoalescedShed,
            SegmentOutcome::Cancelled,
        ];
        for o in all {
            match o {
                SegmentOutcome::Ok
                | SegmentOutcome::Short
                | SegmentOutcome::Empty
                | SegmentOutcome::Failed
                | SegmentOutcome::Shed
                | SegmentOutcome::CoalescedFailed
                | SegmentOutcome::CoalescedShed
                | SegmentOutcome::Cancelled => {}
            }
        }
        let labels: Vec<&str> = all.iter().map(|o| o.label()).collect();
        assert_eq!(
            labels,
            vec![
                "ok",
                "short",
                "empty",
                "failed",
                "shed",
                "coalesced_failed",
                "coalesced_shed",
                "cancelled"
            ]
        );
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
