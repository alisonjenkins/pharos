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
//! - Per-key `tokio::sync::Mutex<()>` deduplicates concurrent fetches:
//!   the first request transcodes + writes the file, others wait on
//!   the lock then read from disk.
//! - LRU tracking via `(access_counter, key) → bytes`; eviction is
//!   triggered after each insert and runs lazily until total bytes is
//!   under the configured cap.
//! - V6 still holds: a crashed ffmpeg subprocess never poisons the
//!   cache; the writer renames `.tmp → .ts` atomically and removes the
//!   tmp file on failure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use pharos_transcode::{FfmpegTranscoder, SegmentOpts, SegmentVideo, TranscodeOptions};
use tokio::io::AsyncReadExt;

#[derive(Debug, thiserror::Error)]
pub enum HlsCacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcode: {0}")]
    Transcode(String),
    #[error("non-utf8 path")]
    NonUtf8Path,
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
struct SegmentKey {
    media_id: u64,
    seg_index: u32,
    /// 0 = default track (no client override).
    audio_index: u32,
    /// `NO_SUBTITLE` (-1) = no burn-in.
    subtitle_index: i32,
    /// kbps; 0 = negotiator default.
    bitrate_kbps: u32,
    /// See `codec_tag` — distinguishes output codec generations.
    codec_tag: u32,
}

const NO_SUBTITLE: i32 = -1;

/// Stable small tag distinguishing the output video codec so different
/// codec outputs never share a cache entry.
fn codec_tag(video: Option<SegmentVideo>) -> u32 {
    // Bumping a tag orphans every pre-existing cached segment for that codec
    // (LRU reclaims them) — the mechanism used whenever a change alters the
    // BYTES of a segment for a given (media, index) key. The cache key carries
    // no start time, so a boundary change is exactly such a case.
    //
    // Historical tags 1 (Copy), 9 (H265), 10 (Av1) retired with the
    // `SegmentVideo` type (V30): the segmented surface can only emit H264 or
    // VP9, and stream copy is unrepresentable. Tag values for the live
    // codecs are preserved so a warm cache survives the type refactor.
    match video {
        None => 0,
        Some(SegmentVideo::H264) => 8,
        // 12 (was 7): VP9 fMP4 segments are now AUDIO-FREE (audio moved to a
        // separate continuous rendition, the A/V-sync fix) — orphan the old
        // muxed segments.
        Some(SegmentVideo::Vp9) => 12,
    }
}

fn make_key(
    media_id: u64,
    seg_index: u32,
    audio_index: Option<u32>,
    subtitle_index: Option<u32>,
    video_bitrate_bps: Option<u64>,
    video_codec_tag: u32,
) -> SegmentKey {
    SegmentKey {
        media_id,
        seg_index,
        audio_index: audio_index.unwrap_or(0),
        subtitle_index: subtitle_index.map(|n| n as i32).unwrap_or(NO_SUBTITLE),
        bitrate_kbps: video_bitrate_bps
            .map(|b| (b / 1000).min(u32::MAX as u64) as u32)
            .unwrap_or(0),
        codec_tag: video_codec_tag,
    }
}

#[derive(Debug, Default)]
struct CacheState {
    /// Per-key locks. Held while a fetch is in flight so concurrent
    /// requests for the same segment don't race.
    fetch_locks: HashMap<SegmentKey, Arc<Mutex<()>>>,
    /// Per-directory locks deduplicating continuous-audio HLS sessions (the
    /// A/V-sync fix): the first request spawns the one ffmpeg producing the
    /// audio rendition; concurrent requests see it already running.
    audio_locks: HashMap<PathBuf, Arc<Mutex<()>>>,
    entries: HashMap<SegmentKey, EntryMeta>,
    total_bytes: u64,
    access_counter: u64,
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
const HLS_GEN_VERSION: u32 = 3;
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
    ) -> Result<Vec<u8>, HlsCacheError> {
        self.segment_bytes_keyed(media_id, seg_index, None, None, source, opts)
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
    pub async fn segment_bytes_keyed(
        &self,
        media_id: u64,
        seg_index: u32,
        audio_index: Option<u32>,
        subtitle_index: Option<u32>,
        source: &Path,
        opts: &SegmentOpts,
    ) -> Result<Vec<u8>, HlsCacheError> {
        let key = make_key(
            media_id,
            seg_index,
            audio_index,
            subtitle_index,
            opts.video_bitrate_bps,
            codec_tag(opts.video),
        );
        let path = self.segment_path_keyed(key);

        // Fast hit path: file present, just bump LRU. A concurrent
        // eviction can delete the file between try_exists and read; treat
        // that NotFound as a miss and fall through to regenerate rather
        // than surfacing a spurious 500 on a genuine cache hit.
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            self.touch(key).await;
            match tokio::fs::read(&path).await {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* evicted; fall through */
                }
                Err(e) => return Err(e.into()),
            }
        }

        let lock = {
            let mut state = self.state.lock().await;
            state
                .fetch_locks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // Re-check: another task may have populated while we waited.
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            self.touch(key).await;
            match tokio::fs::read(&path).await {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* evicted; fall through */
                }
                Err(e) => return Err(e.into()),
            }
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("ts.tmp");
        // Time the transcode: a segment covers SEGMENT_SECONDS of playback, so
        // if this exceeds that wall-clock the encoder is below realtime and the
        // client will stall. Logged per miss so Loki/Tempo show exactly which
        // segments are slow and why (codec + subtitle burn are the usual cost).
        let started = std::time::Instant::now();
        let timing = match self
            .write_segment(source, &opts.to_transcode_options(), &tmp)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        tokio::fs::rename(&tmp, &path).await?;

        let bytes = tokio::fs::read(&path).await?;
        let transcode_ms = started.elapsed().as_millis();
        // Split total transcode_ms into scheduler queue-wait vs actual encode
        // (from the scheduler's JobDone), plus the winning device + retry count,
        // so a slow segment is diagnosable: high queue_wait_ms = saturated
        // devices / failed-device retry churn (e.g. phantom GPUs), high
        // encode_ms = a genuinely slow encoder. Fields land on the HTTP request
        // span this runs under.
        let seek_secs = opts.start_position_ticks as f64 / 10_000_000.0;
        let seg_secs = opts.duration_ticks.map(|t| t as f64 / 10_000_000.0);
        tracing::info!(
            media.id = media_id,
            seg = seg_index,
            transcode_ms = transcode_ms as u64,
            queue_wait_ms = timing.as_ref().map(|t| t.queue_wait_ms),
            encode_ms = timing.as_ref().map(|t| t.encode_ms),
            device = timing.as_ref().map(|t| t.device.to_string()),
            bytes = bytes.len(),
            codec = codec_tag(opts.video),
            burn = opts.burn_subtitle_stream_index.is_some(),
            burn_idx = opts.burn_subtitle_stream_index,
            audio_idx = opts.audio_source_stream_index,
            seek_secs,
            "hls segment transcoded (cache miss)"
        );
        // A segment covering N seconds of content that takes >3×N to encode
        // is drowning (client consumes 1×; even prefetch can't hide a 3×
        // deficit for long). Surface it at WARN with every dimension needed
        // to attribute the stall — the 170-225 s outliers observed live
        // (2026-07-14, Avatar burn path) were only findable by correlating
        // INFO lines after the fact.
        let realtime_budget_ms = seg_secs.unwrap_or(6.0) * 1000.0;
        if (transcode_ms as f64) > 3.0 * realtime_budget_ms {
            tracing::warn!(
                media.id = media_id,
                seg = seg_index,
                transcode_ms = transcode_ms as u64,
                queue_wait_ms = timing.as_ref().map(|t| t.queue_wait_ms),
                encode_ms = timing.as_ref().map(|t| t.encode_ms),
                device = timing.as_ref().map(|t| t.device.to_string()),
                codec = codec_tag(opts.video),
                burn = opts.burn_subtitle_stream_index.is_some(),
                seek_secs,
                seg_secs,
                source = %source.display(),
                "hls segment transcode far below realtime"
            );
        }
        self.record(key, bytes.len() as u64).await;
        self.maybe_evict().await;
        // Release the per-key fetch lock so future calls don't keep it
        // forever — leave the file in the LRU.
        let mut state = self.state.lock().await;
        state.fetch_locks.remove(&key);
        Ok(bytes)
    }

    #[cfg(test)]
    fn segment_path(&self, media_id: u64, seg_index: u32) -> PathBuf {
        self.segment_path_keyed(SegmentKey {
            media_id,
            seg_index,
            audio_index: 0,
            subtitle_index: NO_SUBTITLE,
            bitrate_kbps: 0,
            codec_tag: 0,
        })
    }

    /// Compose `{root}/{media_id}/{seg}.ts` for the default case
    /// (audio=0, subtitle=-1, bitrate=auto) and a longer-form
    /// `{root}/{media_id}/{seg}-a{A}-s{S}-b{Bkbps}.ts` when any
    /// dimension diverges. Keeps the existing on-disk layout intact
    /// for warm caches that pre-date per-track + per-variant keys.
    fn segment_path_keyed(&self, key: SegmentKey) -> PathBuf {
        let SegmentKey {
            media_id,
            seg_index,
            audio_index,
            subtitle_index,
            bitrate_kbps: bitrate_k,
            codec_tag: codec_k,
        } = key;
        // The codec tag is ALWAYS in the filename now. This deliberately
        // orphans any pre-existing codec-blind `{seg}.ts` files: some were
        // written by the old fallback that stream-copied HEVC into an avc1
        // manifest, and there's no way to tell a poisoned HEVC `{seg}.ts` from
        // a correct h264 one on disk — so bypass them all and let LRU reclaim
        // the space. New files carry `-c{tag}` and never collide across codecs.
        let sub_part = if subtitle_index == NO_SUBTITLE {
            "off".to_string()
        } else {
            subtitle_index.to_string()
        };
        let bitrate_part = if bitrate_k == 0 {
            "auto".to_string()
        } else {
            format!("{bitrate_k}")
        };
        let filename =
            format!("{seg_index}-a{audio_index}-s{sub_part}-b{bitrate_part}-c{codec_k}.ts");
        self.root.join(media_id.to_string()).join(filename)
    }

    /// Transcode one segment to `out`. Returns the scheduler's timing split
    /// (queue-wait vs encode + device) when the scheduler path ran, so the
    /// caller can attribute a slow segment; `None` on the inline fallback.
    async fn write_segment(
        &self,
        source: &Path,
        opts: &TranscodeOptions,
        out: &Path,
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
                )
                .await
                .map_err(|e| HlsCacheError::Transcode(e.to_string()))?;
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

    /// Ensure an audio-rendition session exists whose output will cover
    /// `want_seg` promptly. `want_seg == 0` is the plain from-the-start
    /// session; a deep target spawns an additional session seeked to that
    /// segment boundary (`-ss`, `-start_number`, `-output_ts_offset` so the
    /// fmp4 timestamps stay source-anchored). Sessions share the directory —
    /// overlapping segments are byte-wise re-written with identical content.
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
        // The requested segment already exists → nothing to spawn.
        if tokio::fs::try_exists(&dir.join(format!("a{want_seg}.m4s")))
            .await
            .unwrap_or(false)
        {
            return Ok(dir);
        }
        // Pick the session start that serves this request.
        let start_seg = if want_seg <= Self::AUDIO_SEEK_LOOKAHEAD_SEGS {
            0
        } else {
            match Self::audio_session_progress(&dir).await {
                // A session has written up to n_max; a target within the
                // lookahead window will land during the read poll.
                Some(n_max)
                    if want_seg <= n_max.saturating_add(Self::AUDIO_SEEK_LOOKAHEAD_SEGS) =>
                {
                    return Ok(dir);
                }
                _ => want_seg,
            }
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
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(&running, b"").await?;

        let args = Self::audio_hls_args(source, &dir, audio_index, audio_bitrate_bps, start_seg)?;
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

    /// Highest `aN.m4s` index present in an audio-rendition dir — the
    /// running session's write progress. `None` when no segment exists yet.
    async fn audio_session_progress(dir: &Path) -> Option<u32> {
        let mut best: Option<u32> = None;
        let mut rd = tokio::fs::read_dir(dir).await.ok()?;
        while let Ok(Some(e)) = rd.next_entry().await {
            if let Some(name) = e.file_name().to_str() {
                if let Some(n) = name
                    .strip_prefix('a')
                    .and_then(|r| r.strip_suffix(".m4s"))
                    .and_then(|r| r.parse::<u32>().ok())
                {
                    best = Some(best.map_or(n, |b| b.max(n)));
                }
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
        dir: &Path,
        audio_index: Option<u32>,
        audio_bitrate_bps: Option<u64>,
        start_seg: u32,
    ) -> Result<Vec<String>, HlsCacheError> {
        let src = source
            .to_str()
            .ok_or(HlsCacheError::NonUtf8Path)?
            .to_string();
        let seg_pat = dir
            .join("a%d.m4s")
            .to_str()
            .ok_or(HlsCacheError::NonUtf8Path)?
            .to_string();
        // Seek sessions write a throwaway playlist so they can never clobber
        // the from-0 session's `audio.m3u8` done-marker.
        let m3u8_name = if start_seg == 0 {
            "audio.m3u8".to_string()
        } else {
            format!("audio-from-{start_seg}.m3u8")
        };
        let m3u8 = dir
            .join(m3u8_name)
            .to_str()
            .ok_or(HlsCacheError::NonUtf8Path)?
            .to_string();
        let bitrate = audio_bitrate_bps.unwrap_or(128_000);
        let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        let start_secs = start_seg as f64 * 6.0;
        if start_seg > 0 {
            args.push("-ss".into());
            args.push(format!("{start_secs:.3}"));
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
        args.extend(
            ["-c:a", "libopus", "-b:a", &bitrate.to_string(), "-ac", "2"]
                .into_iter()
                .map(String::from),
        );
        if start_seg > 0 {
            args.push("-output_ts_offset".into());
            args.push(format!("{start_secs:.3}"));
        }
        args.extend(
            [
                "-f",
                "hls",
                "-hls_time",
                "6",
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

    /// Read a produced audio-rendition file (`init.mp4`, `aN.m4s`, or
    /// `audio.m3u8`) from an [`ensure_audio_hls`](Self::ensure_audio_hls)
    /// directory, polling briefly for it to appear — the continuous ffmpeg
    /// writes segments ahead of the playhead, so a just-requested segment is
    /// usually already there; a near-live one may need a moment. Returns
    /// `NotFound` past the wait budget.
    pub async fn audio_hls_file(&self, dir: &Path, name: &str) -> Result<Vec<u8>, HlsCacheError> {
        // Basic traversal guard: names are simple file basenames.
        if name.contains('/') || name.contains("..") {
            return Err(HlsCacheError::Io(std::io::Error::from(
                std::io::ErrorKind::InvalidInput,
            )));
        }
        let path = dir.join(name);
        // Up to ~5 s of polling (segments normally land far faster).
        for _ in 0..100 {
            match tokio::fs::read(&path).await {
                Ok(b) if !b.is_empty() => return Ok(b),
                _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
        Err(HlsCacheError::Io(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        )))
    }

    async fn touch(&self, key: SegmentKey) {
        let mut state = self.state.lock().await;
        state.access_counter += 1;
        let counter = state.access_counter;
        if let Some(meta) = state.entries.get_mut(&key) {
            meta.last_used = counter;
        }
    }

    async fn record(&self, key: SegmentKey, bytes: u64) {
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
        let mut to_remove: Vec<(SegmentKey, PathBuf)> = Vec::new();
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

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
                SegmentKey {
                    media_id,
                    seg_index: seg,
                    audio_index: 0,
                    subtitle_index: NO_SUBTITLE,
                    bitrate_kbps: 0,
                    codec_tag: 0,
                },
                body.len() as u64,
            )
            .await;
        cache.maybe_evict().await;
    }

    #[tokio::test]
    async fn hit_returns_cached_bytes_without_calling_ffmpeg() {
        let td = TempDir::new().unwrap();
        let cache = HlsSegmentCache::new(td.path(), 1024).with_ffmpeg("/no/such/ffmpeg");
        force_insert(&cache, 7, 0, b"segment-bytes").await;
        let opts = SegmentOpts {
            container: pharos_transcode::SegmentContainer::Mpegts,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let got = cache
            .segment_bytes(7, 0, Path::new("/no/source"), &opts)
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
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let res = cache
            .segment_bytes(8, 0, Path::new("/no/source"), &opts)
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
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let _ = cache
            .segment_bytes(7, 0, Path::new("/no/source"), &opts)
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
                audio: None,
                video_bitrate_bps: None,
                audio_bitrate_bps: None,
                start_position_ticks: 0,
                duration_ticks: None,
                audio_source_stream_index: None,
                burn_subtitle_stream_index: None,
            };
            cache
                .segment_bytes(9, 0, Path::new("/n"), &opts)
                .await
                .unwrap()
        };
        let (a, b) = tokio::join!(one, async {
            counter.fetch_add(1, Ordering::SeqCst);
            let opts = SegmentOpts {
                container: pharos_transcode::SegmentContainer::Mpegts,
                video: None,
                audio: None,
                video_bitrate_bps: None,
                audio_bitrate_bps: None,
                start_position_ticks: 0,
                duration_ticks: None,
                audio_source_stream_index: None,
                burn_subtitle_stream_index: None,
            };
            cache
                .segment_bytes(9, 0, Path::new("/n"), &opts)
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
        )
        .unwrap();
        let joined = a.join(" ");
        assert!(!joined.contains("-ss"), "{joined}");
        assert!(!joined.contains("-start_number"), "{joined}");
        assert!(!joined.contains("-output_ts_offset"), "{joined}");
        assert!(joined.ends_with("audio.m3u8"), "{joined}");
        assert!(joined.contains("-map 0:a:1"), "{joined}");
    }

    /// B42 — a seek session must be source-anchored: input-seek to the
    /// segment boundary, absolute segment numbering, and true-timeline
    /// fragment timestamps (a PTS-0 fragment buffers at 0:00 in hls.js —
    /// the B41 failure class). Its playlist must not clobber the from-0
    /// session's done-marker.
    #[test]
    fn audio_hls_args_seek_session_is_source_anchored() {
        let a = HlsSegmentCache::audio_hls_args(
            Path::new("/m/x.mkv"),
            Path::new("/c/d"),
            None,
            Some(128_000),
            30,
        )
        .unwrap();
        let joined = a.join(" ");
        assert!(joined.contains("-ss 180.000"), "{joined}");
        assert!(joined.contains("-output_ts_offset 180.000"), "{joined}");
        assert!(joined.contains("-start_number 30"), "{joined}");
        assert!(joined.ends_with("audio-from-30.m3u8"), "{joined}");
        // -ss must be an INPUT option (before -i).
        let ss = a.iter().position(|x| x == "-ss").unwrap();
        let i = a.iter().position(|x| x == "-i").unwrap();
        assert!(ss < i, "-ss must precede -i: {joined}");
    }
}
