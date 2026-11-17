//! Spec 040 (ADR-0122): checks each text subtitle track against its video
//! with the lyric aligner, and stores a verdict per track.
//!
//! # Shape
//!
//! The mould of [`crate::subtitle_styling`]: after a warm-up and the
//! background leader, a pass over the library every hour, one track at a time.
//!
//! - **Films first** (FR-016a), then episodes.
//! - **Once per file** (FR-015). A row per item and track source, keyed by the
//!   video's and the sidecar's signature and [`CHECK_VERSION`]; only a changed
//!   file or a new version is checked again.
//! - **Karaoke first** (FR-016). No request while a song is queued or being
//!   aligned, or while a viewer's transcode waits; one request at a time.
//! - **An aligner failure is not a result** (FR-015a). It stores an attempt
//!   count and a retry time an hour on, and only after three attempts an
//!   `unknown` verdict. While `/healthz` fails the job pauses.
//! - **Two stages** (FR-007, FR-008). Stage 1 sends the probes at their own
//!   times. A track that does not fit goes to stage 2: the coarse search
//!   ([`pharos_subs::check::coarse`]) decodes the audio once at 8 kHz, finds
//!   where the cue pattern lines up with speech within ±120 s and at four speed
//!   ratios, and the best [`MAX_STAGE2`] of those are tried with the aligner
//!   until one fits. Only stage 2 can call a track `wrong_video`.
//! - **Log only.** Nothing reads the rows to change what is served until
//!   `[subtitle_check].apply` and slice 5.
//!
//! # Signals
//!
//! ```text
//! pharos_subtitle_check_total{verdict,reason_class,stage}  stage: gate | 1 | 2
//! pharos_subtitle_check_aligner_seconds                    aligner time per check
//! pharos_subtitle_check_coarse_seconds                     coarse search per stage-2 check
//! pharos_subtitle_check_pending                            tracks left at the start of a pass
//! ```
//!
//! Every track leaves one `subtitle check` log line with its item, source,
//! languages, verdict, reason, every probe and decoy score at each timing
//! tried, the aligner time and the coarse-search time; a failure carries the
//! same fields.

use crate::bg_io::BgPermit;
use crate::services::lyrics::align::{Aligned, Aligner};
use crate::subtitle_styling::{
    candidates_for, read_source, signature, SubtitleCandidate, TranscodeQueue,
};
use pharos_cache::SubtitleCache;
use pharos_core::{
    AudioTrack, MediaItem, MediaKind, MediaStore, SubtitleCheckRow, SubtitleCheckStore,
};
use pharos_subs::check::coarse::{self, Shift, Speech};
use pharos_subs::check::lang;
use pharos_subs::check::probes::{self, Probe};
use pharos_subs::check::verdict::{self, Candidate, Thresholds, UnknownClass, Verdict};
use pharos_subs::Script;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Bumped when the probe picker, the rules or the request change, so every
/// track is checked again.
pub const CHECK_VERSION: u32 = 2;
/// FR-007: each probe's window ends this much after the cue.
pub const PAD_MS: u64 = 2_000;
/// FR-015a: an aligner failure is retried no sooner than this (seconds), as
/// the karaoke queue's `RETRY_AFTER`.
pub const RETRY_AFTER_SECS: i64 = 3_600;
/// FR-015a: attempts before an aligner failure is stored as `unknown`.
pub const MAX_ATTEMPTS: u32 = 3;
/// FR-006: stage-2 candidates tried, two requests each, so a track costs at
/// most 8 requests with stage 1's two.
pub const MAX_STAGE2: usize = 3;
/// How far the aligner's own offset search reaches (±8 s): stage 1 has looked
/// there already, so stage 2 does not try a ratio-1 offset inside it.
pub const ALIGNER_REACH_MS: i64 = 8_000;

/// Delay before the first pass, so boot I/O settles (as the other sweeps).
const WARMUP: Duration = Duration::from_secs(240);
/// Between passes; a pass over a checked library reads no subtitle.
const PASS_INTERVAL: Duration = Duration::from_secs(3600);
/// How often a waiting check looks at the karaoke and transcode queues.
const BUSY_POLL: Duration = Duration::from_secs(5);
/// How often a paused check looks at the aligner's health.
const HEALTH_POLL: Duration = Duration::from_secs(60);

/// What the job needs from the aligner. [`Aligner`] in production.
pub trait Scorer: Send + Sync {
    /// A karaoke song is queued or being aligned.
    fn busy(&self) -> bool;
    fn healthy(&self) -> impl Future<Output = bool> + Send;
    fn score(
        &self,
        path: &Path,
        lines: &[(u64, u64, String)],
    ) -> impl Future<Output = Result<Aligned, String>> + Send;
}

impl Scorer for Arc<Aligner> {
    fn busy(&self) -> bool {
        self.karaoke_busy()
    }
    async fn healthy(&self) -> bool {
        Aligner::healthy(self).await
    }
    async fn score(&self, path: &Path, lines: &[(u64, u64, String)]) -> Result<Aligned, String> {
        Aligner::score(self, path, lines).await
    }
}

/// FR-009: hears where a file's audio holds speech. [`FfmpegEars`] in
/// production.
pub trait Listen: Send + Sync {
    /// The speech map of `stream` (an absolute stream index), or of the first
    /// audio stream when `None`.
    fn speech(
        &self,
        path: &Path,
        stream: Option<u32>,
    ) -> impl Future<Output = Result<Speech, String>> + Send;
}

/// Decodes with the `ffmpeg` binary at 8 kHz mono and folds the samples into
/// a [`coarse::Listener`] as they arrive, so the audio is never held.
pub struct FfmpegEars;

/// Bytes of ffmpeg's stderr kept for an error message.
const STDERR_TAIL: usize = 2_048;

impl Listen for FfmpegEars {
    async fn speech(&self, path: &Path, stream: Option<u32>) -> Result<Speech, String> {
        use std::process::Stdio;
        use tokio::io::AsyncReadExt;
        let map = stream.map_or_else(|| "0:a:0".to_owned(), |i| format!("0:{i}"));
        let mut child = tokio::process::Command::new(pharos_transcode::worker::exec::ffmpeg_bin())
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(path)
            .args(["-map", &map, "-vn", "-sn", "-dn", "-ac", "1", "-ar"])
            .arg(coarse::RATE.to_string())
            .args(["-f", "s16le", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn: {e}"))?;
        let mut stderr = child.stderr.take().ok_or("no stderr")?;
        let errs = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf).await;
            let start = buf.len().saturating_sub(STDERR_TAIL);
            String::from_utf8_lossy(&buf[start..]).trim().to_owned()
        });
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let mut ears = coarse::Listener::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut carry: Option<u8> = None;
        let mut samples: Vec<i16> = Vec::with_capacity(buf.len() / 2 + 1);
        loop {
            let n = stdout
                .read(&mut buf)
                .await
                .map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                break;
            }
            samples.clear();
            let mut bytes = &buf[..n];
            if let Some(lo) = carry.take() {
                samples.push(i16::from_le_bytes([lo, bytes[0]]));
                bytes = &bytes[1..];
            }
            let mut pairs = bytes.chunks_exact(2);
            samples.extend(pairs.by_ref().map(|b| i16::from_le_bytes([b[0], b[1]])));
            carry = pairs.remainder().first().copied();
            ears.push(&samples);
        }
        let status = child.wait().await.map_err(|e| format!("wait: {e}"))?;
        let errs = errs.await.unwrap_or_default();
        if !status.success() {
            return Err(format!("ffmpeg {status}: {errs}"));
        }
        Ok(ears.finish())
    }
}

pub trait CheckStore: MediaStore + SubtitleCheckStore {}
impl<S: MediaStore + SubtitleCheckStore> CheckStore for S {}

/// The `stage` label: where a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// FR-002 to FR-004: decided from the files, no aligner request.
    Gate,
    /// FR-007: the probes at their own times.
    One,
    /// FR-008: the wide search.
    Two,
}

impl Stage {
    pub const ALL: [Stage; 3] = [Stage::Gate, Stage::One, Stage::Two];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Gate => "gate",
            Stage::One => "1",
            Stage::Two => "2",
        }
    }
}

/// `stream:<index>` for an embedded stream; `sidecar:<hash>` of the sidecar's
/// file name, so a new sidecar beside it (which moves the indices) keeps the key.
pub fn source_key(c: &SubtitleCandidate) -> String {
    match c.sidecar.as_deref().and_then(Path::file_name) {
        Some(name) => {
            // FNV-1a: stable across builds, unlike the std hasher.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in name.as_encoded_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            format!("sidecar:{h:016x}")
        }
        None => format!("stream:{}", c.index),
    }
}

/// The language of the audio a track plays over (FR-002): the default audio
/// stream, else the first.
pub fn audio_language(item: &MediaItem) -> Option<&str> {
    audio_stream(item).and_then(|t| t.language.as_deref())
}

/// The audio stream a track is checked against: the default, else the first.
pub fn audio_stream(item: &MediaItem) -> Option<&AudioTrack> {
    let tracks = &item.probe.audio_tracks;
    tracks
        .iter()
        .find(|t| t.is_default)
        .or_else(|| tracks.first())
}

/// Films, then episodes (FR-016a), each in library order.
pub fn backfill_order(items: &[MediaItem]) -> Vec<&MediaItem> {
    let local = |i: &&MediaItem| i.origin().local().is_some();
    let of = |kind: MediaKind| items.iter().filter(move |i| i.kind == kind).filter(local);
    of(MediaKind::Movie).chain(of(MediaKind::Episode)).collect()
}

/// The signatures a row is current for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sigs {
    video: (i64, u64),
    sidecar: Option<(i64, u64)>,
}

impl Sigs {
    fn matches(&self, row: &SubtitleCheckRow) -> bool {
        row.check_version == CHECK_VERSION
            && (row.source_mtime, row.source_size) == self.video
            && row.sidecar_mtime.zip(row.sidecar_size) == self.sidecar
    }
}

/// What to do with a track, given its stored row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Never checked, or checked against other files or another version.
    Fresh,
    /// An aligner failure whose retry time has come; carries the attempts so far.
    Retry(u32),
    /// Finished, or a retry not yet due.
    No,
}

fn due(row: Option<&SubtitleCheckRow>, sigs: &Sigs, now: i64) -> Due {
    match row {
        Some(r) if sigs.matches(r) => match (&r.verdict, r.retry_at) {
            (Some(_), _) => Due::No,
            (None, Some(at)) if at > now => Due::No,
            (None, _) => Due::Retry(r.attempts),
        },
        _ => Due::Fresh,
    }
}

/// How one track's check ended, for the pass summary.
#[derive(Debug, Clone, PartialEq)]
enum Ended {
    Verdict(Verdict, Stage),
    /// The aligner failed and the check waits for its retry.
    Retry(String),
    /// Nothing stored: the source could not be read, or the store refused.
    Skipped(String),
}

/// Totals for one pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassSummary {
    pub items: usize,
    pub tracks: usize,
    /// Verdict label to count.
    pub verdicts: std::collections::BTreeMap<&'static str, usize>,
    pub retries: usize,
    pub skipped: usize,
}

/// The job. Fields are public for the tests.
pub struct Checker<S, A, Q, L> {
    pub store: S,
    pub aligner: A,
    pub queue: Q,
    pub ears: L,
    pub bg_io: Arc<Semaphore>,
    pub cache: Option<SubtitleCache>,
    pub thresholds: Thresholds,
    pub busy_poll: Duration,
    pub health_poll: Duration,
}

/// Spawn the job when an aligner is configured and the job is enabled
/// (FR-018). Fire-and-forget: a failure aborts only the job, never the server.
pub fn spawn<S, Q>(
    store: S,
    bg_leader: Arc<std::sync::atomic::AtomicBool>,
    bg_io: Arc<Semaphore>,
    cache: Option<SubtitleCache>,
    queue: Q,
    aligner: Option<Arc<Aligner>>,
    cfg: &crate::config::SubtitleCheckConfig,
) where
    S: CheckStore + 'static,
    Q: TranscodeQueue + 'static,
{
    let Some(aligner) = aligner else {
        tracing::info!("subtitle check: off, no [lyrics].aligner_url");
        return;
    };
    if !cfg.enabled {
        tracing::info!("subtitle check: off, [subtitle_check].enabled = false");
        return;
    }
    let thresholds = cfg.thresholds();
    tracing::info!(
        check_version = CHECK_VERSION,
        median = thresholds.median,
        line = thresholds.line,
        margin = thresholds.margin,
        apply = cfg.apply,
        "subtitle check: spawning the background job"
    );
    let job = Checker {
        store,
        aligner,
        queue,
        ears: FfmpegEars,
        bg_io,
        cache,
        thresholds,
        busy_poll: BUSY_POLL,
        health_poll: HEALTH_POLL,
    };
    tokio::spawn(async move {
        tokio::time::sleep(WARMUP).await;
        while !bg_leader.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        loop {
            job.run_pass().await;
            tokio::time::sleep(PASS_INTERVAL).await;
        }
    });
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A probe's window (FR-007): the cue's own start, so the aligner's offset is
/// measured from it, and the end padded by [`PAD_MS`]. A padded start would
/// shift every offset by the pad, and the aligner zeroes an offset under
/// 400 ms before the pad could be taken off again.
fn window(p: &Probe) -> (u64, u64) {
    let start = u64::try_from(p.start_ms).unwrap_or(0);
    let end = u64::try_from(p.end_ms).unwrap_or(0);
    (start, end.saturating_add(PAD_MS))
}

/// A probe's window at a stage-2 timing: `video = sub * ratio + offset`, end
/// padded as in [`window`]. `None` when it would start before the file.
fn window_at(p: &Probe, at: Option<Shift>) -> Option<(u64, u64)> {
    let Some(s) = at else {
        return Some(window(p));
    };
    let moved = |ms: i64| -> Option<u64> {
        let scaled = (ms as f64 * s.ratio).round();
        // A film's milliseconds fit an i64; the cast saturates otherwise.
        #[allow(clippy::cast_possible_truncation)]
        let scaled = scaled as i64;
        u64::try_from(scaled.saturating_add(s.offset_ms)).ok()
    };
    Some((moved(p.start_ms)?, moved(p.end_ms)?.saturating_add(PAD_MS)))
}

/// A timing with nothing sent: every score NaN.
fn unsent(n: usize, offset_ms: i64, ratio: f64) -> Candidate {
    Candidate {
        offset_ms,
        ratio,
        probes: vec![f32::NAN; n],
        decoys: vec![f32::NAN; n],
    }
}

/// What one timing's requests came back with.
enum Asked {
    Scored(Candidate),
    /// The decoy request failed; the probes' scores are kept as evidence.
    ProbesOnly(Candidate, String),
    Failed(String),
    /// No probe falls inside the file at this timing.
    Outside,
}

/// Scores in thousandths to `f32`; a line with no score is NaN, which the
/// verdict counts as 0 and never as a hit.
fn scores(a: &Aligned, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            a.scores
                .get(i)
                .copied()
                .flatten()
                .map_or(f32::NAN, |s| f32::from(s) / 1000.0)
        })
        .collect()
}

fn record(v: &Verdict, stage: Stage) {
    metrics::counter!(
        "pharos_subtitle_check_total",
        "verdict" => v.label(),
        "reason_class" => v.reason_class(),
        "stage" => stage.label(),
    )
    .increment(1);
}

/// What one track's check measured, for its row and log line.
#[derive(Debug, Default)]
struct Measured {
    stage1: Option<Candidate>,
    /// FR-008: each stage-2 candidate tried, with the coarse match it came from.
    stage2: Vec<(Shift, Candidate)>,
    aligner_s: f64,
    /// FR-009: time spent decoding and matching, when stage 2 ran.
    coarse_s: Option<f64>,
}

impl Measured {
    fn scores_json(&self) -> Option<String> {
        let c = self.stage1.as_ref()?;
        let round = |v: f64| (v * 1000.0).round() / 1000.0;
        let finite = |v: &[f32]| -> Vec<Option<f64>> {
            v.iter()
                .map(|s| s.is_finite().then(|| round(f64::from(*s))))
                .collect()
        };
        let mut stages = vec![serde_json::json!({
            "stage": 1,
            "offset_ms": c.offset_ms,
            "ratio": c.ratio,
            "probes": finite(&c.probes),
            "decoys": finite(&c.decoys),
        })];
        stages.extend(self.stage2.iter().map(|(shift, c)| {
            serde_json::json!({
                "stage": 2,
                "coarse_offset_ms": shift.offset_ms,
                "lift": round(shift.lift),
                "offset_ms": c.offset_ms,
                "ratio": c.ratio,
                "probes": finite(&c.probes),
                "decoys": finite(&c.decoys),
            })
        }));
        Some(serde_json::json!({ "stages": stages }).to_string())
    }

    /// The stage-2 timings tried, for the log: `offset@ratio:median/decoy`
    /// and every probe and decoy score (FR-020), `; `-separated.
    fn stage2_summary(&self, t: &Thresholds) -> String {
        self.stage2
            .iter()
            .map(|(_, c)| {
                let f = c.fit(t);
                format!(
                    "{}@{:.5}:{:.2}/{:.2} probes={:?} decoys={:?}",
                    c.offset_ms, c.ratio, f.median, f.decoy, c.probes, c.decoys
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl<S: CheckStore, A: Scorer, Q: TranscodeQueue, L: Listen> Checker<S, A, Q, L> {
    /// One pass over the library.
    pub async fn run_pass(&self) -> PassSummary {
        let mut sum = PassSummary::default();
        let items = match self.store.list().await {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!(error = %e, "subtitle check: item list failed");
                return sum;
            }
        };
        let index: HashMap<(u64, String), SubtitleCheckRow> =
            match self.store.subtitle_check_index().await {
                Ok(rows) => rows
                    .into_iter()
                    .map(|r| ((r.item_id, r.source_key.clone()), r))
                    .collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "subtitle check: index read failed");
                    return sum;
                }
            };
        let now = now_secs();
        let mut work: Vec<(&MediaItem, SubtitleCandidate, Sigs, Due)> = Vec::new();
        for item in backfill_order(&items) {
            let Some(video) = signature(&item.path).await else {
                continue;
            };
            for c in candidates_for(item)
                .await
                .into_iter()
                .filter(|c| c.is_text())
            {
                let sidecar = match c.sidecar.as_deref() {
                    Some(p) => match signature(p).await {
                        Some(s) => Some(s),
                        None => continue,
                    },
                    None => None,
                };
                let sigs = Sigs { video, sidecar };
                let row = index.get(&(item.id, source_key(&c)));
                match due(row, &sigs, now) {
                    Due::No => {}
                    d => work.push((item, c, sigs, d)),
                }
            }
        }
        metrics::gauge!("pharos_subtitle_check_pending").set(work.len() as f64);
        if work.is_empty() {
            return sum;
        }
        tracing::info!(pending = work.len(), "subtitle check: pass starting");
        let mut last_item = None;
        for (item, c, sigs, d) in work {
            if last_item != Some(item.id) {
                sum.items += 1;
                last_item = Some(item.id);
            }
            sum.tracks += 1;
            match self.check_track(item, &c, sigs, d).await {
                Ended::Verdict(v, _) => *sum.verdicts.entry(v.label()).or_default() += 1,
                Ended::Retry(_) => sum.retries += 1,
                Ended::Skipped(_) => sum.skipped += 1,
            }
        }
        tracing::info!(
            items = sum.items,
            tracks = sum.tracks,
            verdicts = ?sum.verdicts,
            retries = sum.retries,
            skipped = sum.skipped,
            "subtitle check: pass complete"
        );
        sum
    }

    /// FR-016: wait for karaoke and viewers' transcodes; FR-015a: pause while
    /// the aligner is down.
    async fn wait_for_turn(&self) {
        loop {
            if self.aligner.busy() {
                tokio::time::sleep(self.busy_poll).await;
                continue;
            }
            self.wait_for_transcodes().await;
            if !self.aligner.healthy().await {
                tracing::info!("subtitle check: aligner unhealthy; paused");
                tokio::time::sleep(self.health_poll).await;
                continue;
            }
            // A song may have been queued while the transcodes or the health
            // check were waited on: look again before sending.
            if self.aligner.busy() {
                continue;
            }
            return;
        }
    }

    /// FR-016: start no track while a viewer's transcode waits.
    async fn wait_for_transcodes(&self) {
        while self.queue.pending().await > 0 {
            tokio::time::sleep(self.busy_poll).await;
        }
    }

    async fn check_track(
        &self,
        item: &MediaItem,
        c: &SubtitleCandidate,
        sigs: Sigs,
        d: Due,
    ) -> Ended {
        let started = Instant::now();
        let key = source_key(c);
        let audio_lang = audio_language(item);
        self.wait_for_transcodes().await;
        let text = {
            let permit = BgPermit::acquire(&self.bg_io).await;
            read_source(self.cache.as_ref(), item, c, &permit).await
        };
        let text = match text {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    item_id = item.id,
                    source = %key,
                    index = c.index,
                    error = %e,
                    elapsed_s = started.elapsed().as_secs_f64(),
                    "subtitle check: source not read; tried again next pass"
                );
                return Ended::Skipped(e);
            }
        };
        let script = Script::parse(&text);
        let mut measured = Measured::default();
        let judged = self
            .judge(item, c, &script, audio_lang, &mut measured)
            .await;
        if measured.aligner_s > 0.0 {
            metrics::histogram!("pharos_subtitle_check_aligner_seconds").record(measured.aligner_s);
        }
        let (verdict, stage) = match judged {
            Ok(v) => v,
            Err(e) => {
                let attempts = match d {
                    Due::Retry(n) => n.saturating_add(1),
                    _ => 1,
                };
                if attempts < MAX_ATTEMPTS {
                    let row = self.row(item, &key, sigs, None, &e, &measured, attempts);
                    let row = SubtitleCheckRow {
                        retry_at: Some(now_secs().saturating_add(RETRY_AFTER_SECS)),
                        ..row
                    };
                    self.log_track(item, c, &key, audio_lang, None, &e, &measured, started);
                    return match self.store.put_subtitle_check(row).await {
                        Ok(()) => Ended::Retry(e),
                        Err(se) => Ended::Skipped(se.to_string()),
                    };
                }
                (
                    Verdict::unknown(
                        UnknownClass::AlignerError,
                        format_args!("aligner_error:{e}"),
                    ),
                    if measured.coarse_s.is_some() {
                        Stage::Two
                    } else {
                        Stage::One
                    },
                )
            }
        };
        record(&verdict, stage);
        let reason = verdict.to_string();
        self.log_track(
            item,
            c,
            &key,
            audio_lang,
            Some(&verdict),
            &reason,
            &measured,
            started,
        );
        let row = self.row(item, &key, sigs, Some(&verdict), &reason, &measured, 0);
        match self.store.put_subtitle_check(row).await {
            Ok(()) => Ended::Verdict(verdict, stage),
            Err(e) => {
                tracing::warn!(item_id = item.id, source = %key, error = %e, "subtitle check: store write failed");
                Ended::Skipped(e.to_string())
            }
        }
    }

    /// The gate, stage 1, then stage 2 for a track that does not fit.
    /// `Err` is an aligner failure.
    async fn judge(
        &self,
        item: &MediaItem,
        c: &SubtitleCandidate,
        script: &Script,
        audio_lang: Option<&str>,
        measured: &mut Measured,
    ) -> Result<(Verdict, Stage), String> {
        let texts = || script.events.iter().map(|e| e.text.as_str());
        if let Err(skip) = lang::gate(c.language.as_deref(), audio_lang, texts()) {
            let class = match skip {
                lang::Skip::LanguageMismatch { .. } => UnknownClass::LanguageMismatch,
                lang::Skip::Script { .. } => UnknownClass::Script,
                lang::Skip::LanguageUntagged => UnknownClass::LanguageUntagged,
            };
            return Ok((Verdict::unknown(class, skip), Stage::Gate));
        }
        let picked = match probes::pick_probes(script) {
            Ok(p) => p,
            Err(few) => {
                return Ok((
                    Verdict::unknown(UnknownClass::TooFewLines, few),
                    Stage::Gate,
                ))
            }
        };
        let asked = self.ask(item, &picked, None, &mut measured.aligner_s).await;
        let stage1 = match asked {
            Asked::Scored(c) => c,
            Asked::ProbesOnly(c, e) => {
                // The probes' evidence is kept even when the decoy request fails.
                measured.stage1 = Some(c);
                return Err(e);
            }
            Asked::Failed(e) => return Err(e),
            // Every cue is past the end of the video at its own time: nothing
            // to score, so stage 1 does not fit and the wide search looks.
            Asked::Outside => unsent(picked.len(), 0, 1.0),
        };
        measured.stage1 = Some(stage1.clone());
        if stage1.fit(&self.thresholds).fits(&self.thresholds) {
            return Ok((verdict::decide(&stage1, None, &self.thresholds), Stage::One));
        }
        // FR-008/FR-009: the wide search.
        let cues: Vec<(i64, i64)> = script
            .events
            .iter()
            .filter(|e| !e.positioned && e.end_ms > e.start_ms)
            .map(|e| (e.start_ms, e.end_ms))
            .collect();
        self.wait_for_transcodes().await;
        let heard_at = Instant::now();
        let speech = {
            let _permit = BgPermit::acquire(&self.bg_io).await;
            self.ears
                .speech(&item.path, audio_stream(item).map(|t| t.stream_index))
                .await
        };
        let shifts = speech.map(|sp| coarse::candidates(&sp, &cues, ALIGNER_REACH_MS, MAX_STAGE2));
        let coarse_s = heard_at.elapsed().as_secs_f64();
        measured.coarse_s = Some(coarse_s);
        metrics::histogram!("pharos_subtitle_check_coarse_seconds").record(coarse_s);
        let shifts = match shifts {
            Ok(s) => s,
            Err(e) => {
                let fit1 = stage1.fit(&self.thresholds);
                return Ok((
                    Verdict::unknown(
                        UnknownClass::Inconclusive,
                        format_args!("inconclusive coarse_error:{e} {fit1}"),
                    ),
                    Stage::Two,
                ));
            }
        };
        for shift in shifts {
            match self
                .ask(item, &picked, Some(shift), &mut measured.aligner_s)
                .await
            {
                Asked::Scored(c) => {
                    let fits = c.fit(&self.thresholds).fits(&self.thresholds);
                    measured.stage2.push((shift, c));
                    if fits {
                        break;
                    }
                }
                Asked::ProbesOnly(c, e) => {
                    measured.stage2.push((shift, c));
                    return Err(e);
                }
                Asked::Failed(e) => return Err(e),
                Asked::Outside => {}
            }
        }
        let tried: Vec<Candidate> = measured.stage2.iter().map(|(_, c)| c.clone()).collect();
        Ok((
            verdict::decide(&stage1, Some(&tried), &self.thresholds),
            Stage::Two,
        ))
    }

    /// FR-006: one timing's two requests, the probes and then their decoys in
    /// the same windows. `at` is `None` for stage 1 (the cues' own times).
    async fn ask(
        &self,
        item: &MediaItem,
        picked: &[Probe],
        at: Option<Shift>,
        aligner_s: &mut f64,
    ) -> Asked {
        let duration = item.probe.duration_ms;
        // `(probe index, start, end)`: a probe outside the file is not sent.
        let windows: Vec<(usize, u64, u64)> = picked
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let (a, b) = window_at(p, at)?;
                duration.map_or(true, |d| a < d).then_some((i, a, b))
            })
            .collect();
        if windows.is_empty() {
            return Asked::Outside;
        }
        let lines = |text: fn(&Probe) -> &str| -> Vec<(u64, u64, String)> {
            windows
                .iter()
                .map(|&(i, a, b)| (a, b, text(&picked[i]).to_owned()))
                .collect()
        };
        // Scores stay in probe order, an unsent probe NaN (scored as 0 and
        // never a hit), so the verdict compares the same probe across timings.
        let place = |a: &Aligned| -> Vec<f32> {
            let mut out = vec![f32::NAN; picked.len()];
            for (&(i, ..), s) in windows.iter().zip(scores(a, windows.len())) {
                out[i] = s;
            }
            out
        };
        let (offset_ms, ratio) = at.map_or((0, 1.0), |s| (s.offset_ms, s.ratio));
        self.wait_for_turn().await;
        let asked = Instant::now();
        let probe_answer = self.aligner.score(&item.path, &lines(|p| &p.text)).await;
        *aligner_s += asked.elapsed().as_secs_f64();
        let probe_answer = match probe_answer {
            Ok(a) => a,
            Err(e) => return Asked::Failed(e),
        };
        let mut cand = Candidate {
            offset_ms: offset_ms.saturating_add(probe_answer.offset_ms),
            ratio,
            probes: place(&probe_answer),
            decoys: Vec::new(),
        };
        // FR-016: one request at a time, and a song may have come in between.
        self.wait_for_turn().await;
        let asked = Instant::now();
        let decoy_answer = self.aligner.score(&item.path, &lines(|p| &p.decoy)).await;
        *aligner_s += asked.elapsed().as_secs_f64();
        match decoy_answer {
            Ok(a) => {
                cand.decoys = place(&a);
                Asked::Scored(cand)
            }
            Err(e) => Asked::ProbesOnly(cand, e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        item: &MediaItem,
        key: &str,
        sigs: Sigs,
        v: Option<&Verdict>,
        reason: &str,
        measured: &Measured,
        attempts: u32,
    ) -> SubtitleCheckRow {
        let (offset_ms, ratio) = match v {
            Some(Verdict::Retimed {
                offset_ms, ratio, ..
            }) => (Some(*offset_ms), Some(*ratio)),
            _ => (None, None),
        };
        SubtitleCheckRow {
            item_id: item.id,
            source_key: key.to_owned(),
            source_mtime: sigs.video.0,
            source_size: sigs.video.1,
            sidecar_mtime: sigs.sidecar.map(|s| s.0),
            sidecar_size: sigs.sidecar.map(|s| s.1),
            check_version: CHECK_VERSION,
            verdict: v.map(|v| v.label().to_owned()),
            reason: reason.to_owned(),
            offset_ms,
            ratio,
            scores_json: measured.scores_json(),
            attempts,
            retry_at: None,
            checked_at: now_secs(),
        }
    }

    /// FR-020: one line per track, the same fields on success and failure.
    #[allow(clippy::too_many_arguments)]
    fn log_track(
        &self,
        item: &MediaItem,
        c: &SubtitleCandidate,
        key: &str,
        audio_lang: Option<&str>,
        v: Option<&Verdict>,
        reason: &str,
        measured: &Measured,
        started: Instant,
    ) {
        let (offset_ms, ratio, probes, decoys) = match &measured.stage1 {
            Some(c) => (
                Some(c.offset_ms),
                Some(c.ratio),
                format!("{:?}", c.probes),
                format!("{:?}", c.decoys),
            ),
            None => (None, None, String::new(), String::new()),
        };
        tracing::info!(
            item_id = item.id,
            source = %key,
            index = c.index,
            sub_lang = c.language.as_deref().unwrap_or("-"),
            audio_lang = audio_lang.unwrap_or("-"),
            verdict = v.map_or("pending", Verdict::label),
            reason_class = v.map_or("aligner_error", Verdict::reason_class),
            reason = %reason,
            stage1_offset_ms = offset_ms,
            stage1_ratio = ratio,
            probe_scores = %probes,
            decoy_scores = %decoys,
            stage2 = %measured.stage2_summary(&self.thresholds),
            aligner_s = measured.aligner_s,
            coarse_s = measured.coarse_s,
            elapsed_s = started.elapsed().as_secs_f64(),
            "subtitle check"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use metrics_util::debugging::DebugValue;
    use pharos_core::{AudioTrack, MediaProbe};
    use pharos_store_sqlx::sqlite::SqliteStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Answers probes with `probe` and decoys with `decoy` (thousandths), or
    /// fails the first `fail` requests. Busy for the first `busy` looks.
    struct Fake {
        probe: u16,
        decoy: u16,
        offset_ms: i64,
        fail: AtomicUsize,
        busy: AtomicUsize,
        /// Looks answered idle before `busy` starts counting.
        busy_skip: AtomicUsize,
        /// `busy` left at each request.
        busy_at_send: Mutex<Vec<usize>>,
        healthy: bool,
        /// Fail only this request (0-based), when set.
        fail_call: Option<usize>,
        calls: Mutex<Vec<(String, usize)>>,
        starts: Mutex<Vec<u64>>,
        /// When set, the video's real timing of [`srt`]'s cues,
        /// `video = sub * ratio + offset`: a probe scores `probe` only when
        /// sent within the aligner's reach of it, and `offset_ms` is the rest.
        truth: Option<(i64, f64)>,
    }

    impl Fake {
        fn new(probe: u16, decoy: u16) -> Self {
            Self {
                probe,
                decoy,
                offset_ms: 0,
                fail: AtomicUsize::new(0),
                busy: AtomicUsize::new(0),
                busy_skip: AtomicUsize::new(0),
                busy_at_send: Mutex::default(),
                healthy: true,
                fail_call: None,
                calls: Mutex::default(),
                starts: Mutex::default(),
                truth: None,
            }
        }
        fn calls(&self) -> Vec<(String, usize)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Scorer for &Fake {
        fn busy(&self) -> bool {
            if self
                .busy_skip
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return false;
            }
            self.busy
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        }
        async fn healthy(&self) -> bool {
            self.healthy
        }
        async fn score(
            &self,
            path: &Path,
            lines: &[(u64, u64, String)],
        ) -> Result<Aligned, String> {
            self.busy_at_send
                .lock()
                .unwrap()
                .push(self.busy.load(Ordering::SeqCst));
            let mut calls = self.calls.lock().unwrap();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // Probes and decoys alternate: the probe request comes first.
            let is_decoy = calls.iter().filter(|(n, _)| *n == name).count() % 2 == 1;
            calls.push((name, lines.len()));
            let call = calls.len() - 1;
            drop(calls);
            self.starts
                .lock()
                .unwrap()
                .extend(lines.iter().map(|l| l.0));
            if self.fail_call == Some(call) {
                return Err("request: timed out".into());
            }
            if self
                .fail
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err("request: connection refused".into());
            }
            let Some((offset, ratio)) = self.truth else {
                let s = if is_decoy { self.decoy } else { self.probe };
                return Ok(Aligned {
                    lines: vec![Vec::new(); lines.len()],
                    offset_ms: self.offset_ms,
                    scores: vec![Some(s); lines.len()],
                });
            };
            // How far each sent line is from where its cue is really spoken.
            let off: Vec<i64> = lines
                .iter()
                .map(|(start, _, text)| {
                    let i: i64 = text
                        .split("line ")
                        .nth(1)
                        .and_then(|t| t.split(' ').next())
                        .and_then(|n| n.parse().ok())
                        .unwrap();
                    let real = ((i * 60_000 + 1_000) as f64 * ratio).round() as i64 + offset;
                    real - i64::try_from(*start).unwrap()
                })
                .collect();
            let near = |o: &i64| o.abs() <= 8_000;
            let scores = off
                .iter()
                .map(|o| {
                    Some(if !is_decoy && near(o) {
                        self.probe
                    } else {
                        self.decoy
                    })
                })
                .collect();
            let rest = off.iter().copied().find(near).unwrap_or(0);
            Ok(Aligned {
                lines: vec![Vec::new(); lines.len()],
                offset_ms: if rest.abs() < 400 { 0 } else { rest },
                scores,
            })
        }
    }

    /// Hears `speech`, and notes the stream asked for.
    struct Ears {
        speech: Result<Speech, String>,
        streams: Mutex<Vec<Option<u32>>>,
    }

    impl Ears {
        fn hearing(speech: Result<Speech, String>) -> Self {
            Self {
                speech,
                streams: Mutex::default(),
            }
        }
        /// No speech: stage 2 finds no candidate.
        fn deaf() -> Self {
            Self::hearing(Ok(Speech::from_frames(vec![false; 15_000])))
        }
    }

    impl Listen for Ears {
        async fn speech(&self, _: &Path, stream: Option<u32>) -> Result<Speech, String> {
            self.streams.lock().unwrap().push(stream);
            self.speech.clone()
        }
    }

    /// Speech where [`srt`]'s cues are spoken at `video = sub * ratio + offset`.
    fn spoken(offset_ms: i64, ratio: f64) -> Speech {
        let mut on = vec![false; 15_000];
        for i in 0..20i64 {
            let at = |ms: i64| ((ms as f64 * ratio).round() as i64 + offset_ms) / 100;
            let (a, b) = (at(i * 60_000 + 1_000), at(i * 60_000 + 4_000));
            for f in a.max(0)..b.min(15_000) {
                on[usize::try_from(f).unwrap()] = true;
            }
        }
        Speech::from_frames(on)
    }

    struct Idle;
    impl TranscodeQueue for Idle {
        async fn pending(&self) -> usize {
            0
        }
    }

    /// Pending for the first `busy` looks.
    struct Queued {
        busy: AtomicUsize,
    }
    impl TranscodeQueue for &Queued {
        async fn pending(&self) -> usize {
            let left = self
                .busy
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
            usize::from(left.is_ok())
        }
    }

    /// Twenty spoken English cues, a minute apart.
    fn srt() -> String {
        let mut s = String::new();
        for i in 0..20u32 {
            let m = i % 60;
            s.push_str(&format!(
                "{}\n00:{m:02}:01,000 --> 00:{m:02}:04,000\nI think that line {i} is the one we want\n\n",
                i + 1
            ));
        }
        s
    }

    struct Lib {
        _dir: TempDir,
        store: SqliteStore,
    }

    /// `(id, file, kind, audio language, sidecar)`; the sidecar is `.en.srt`.
    async fn library(files: &[(u64, &str, MediaKind, &str, Option<&str>)]) -> Lib {
        let dir = TempDir::new().unwrap();
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        for &(id, name, kind, audio, sidecar) in files {
            let path = dir.path().join(name);
            std::fs::write(&path, b"video").unwrap();
            if let Some(text) = sidecar {
                let stem = name.rsplit_once('.').unwrap().0;
                std::fs::write(dir.path().join(format!("{stem}.en.srt")), text).unwrap();
            }
            store
                .put(MediaItem {
                    id,
                    path,
                    title: name.into(),
                    kind,
                    probe: MediaProbe {
                        duration_ms: Some(1_500_000),
                        audio_tracks: vec![AudioTrack {
                            stream_index: 1,
                            language: Some(audio.into()),
                            is_default: true,
                            ..AudioTrack::default()
                        }],
                        ..MediaProbe::default()
                    },
                    ..MediaItem::default()
                })
                .await
                .unwrap();
        }
        Lib { _dir: dir, store }
    }

    fn job<'a>(lib: &Lib, fake: &'a Fake) -> Checker<SqliteStore, &'a Fake, Idle, Ears> {
        job_hearing(lib, fake, Ears::deaf())
    }

    fn job_hearing<'a>(
        lib: &Lib,
        fake: &'a Fake,
        ears: Ears,
    ) -> Checker<SqliteStore, &'a Fake, Idle, Ears> {
        Checker {
            store: lib.store.clone(),
            aligner: fake,
            queue: Idle,
            ears,
            bg_io: Arc::new(Semaphore::new(1)),
            cache: None,
            thresholds: Thresholds::default(),
            busy_poll: Duration::from_millis(1),
            health_poll: Duration::from_millis(1),
        }
    }

    async fn row(lib: &Lib, id: u64) -> SubtitleCheckRow {
        let rows = lib.store.subtitle_checks(id).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows.into_iter().next().unwrap()
    }

    fn counts(snap: &pharos_test_metrics::CumulativeSnapshotter) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| ck.key().name() == "pharos_subtitle_check_total")
            .map(|(ck, _, _, v)| {
                let DebugValue::Counter(c) = v else { panic!() };
                let labels: Vec<String> = ck.key().labels().map(|l| l.value().to_owned()).collect();
                (labels.join(","), c)
            })
            .collect();
        out.sort();
        out
    }

    #[tokio::test]
    async fn a_right_track_fits_once_and_is_not_checked_again() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&rec);
        let _guard = metrics::set_default_local_recorder(&rec);
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(900, 250);
        let sum = job(&lib, &fake).run_pass().await;
        assert_eq!(sum.verdicts.get("fits"), Some(&1), "{sum:?}");
        // One probe request and one decoy request, twelve lines each.
        assert_eq!(
            fake.calls(),
            [("film.mkv".into(), 12), ("film.mkv".into(), 12)]
        );
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("fits"));
        assert_eq!(r.reason, "median=0.90 decoy=0.25 hits=12/12");
        assert!(r.source_key.starts_with("sidecar:"), "{}", r.source_key);
        assert!(r.scores_json.unwrap().contains("\"decoys\""));
        assert_eq!(counts(&snap), [("fits,fits,1".to_owned(), 1)]);

        let again = job(&lib, &fake).run_pass().await;
        assert_eq!(again.tracks, 0);
        assert_eq!(fake.calls().len(), 2, "a finished track is not asked again");
    }

    #[tokio::test]
    async fn a_shifted_fit_is_retimed_and_keeps_its_offset() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let mut fake = Fake::new(900, 250);
        fake.offset_ms = -3_000;
        job(&lib, &fake).run_pass().await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("retimed"));
        assert_eq!((r.offset_ms, r.ratio), (Some(-3_000), Some(1.0)));
    }

    #[tokio::test]
    async fn ffmpeg_ears_hear_where_the_sound_is_and_name_a_failure() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sound.mka");
        // Room tone, then 2 s of a loud voice-like tone, then room tone.
        let status = tokio::process::Command::new(pharos_transcode::worker::exec::ffmpeg_bin())
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("aevalsrc=if(between(t\\,2\\,4)\\,0.5\\,0.0005)*sin(2*PI*220*t):s=48000:d=6")
            .args(["-c:a", "pcm_s16le"])
            .arg(&path)
            .status()
            .await
            .expect("ffmpeg must be on PATH inside the devShell (see CLAUDE.md)");
        assert!(status.success());
        let speech = FfmpegEars.speech(&path, None).await.unwrap();
        assert_eq!(speech.frames(), 60);
        assert!((19..=21).contains(&speech.speech_frames()), "{speech:?}");

        let err = FfmpegEars
            .speech(&dir.path().join("missing.mkv"), Some(1))
            .await
            .unwrap_err();
        assert!(err.contains("missing.mkv"), "{err}");
    }

    #[tokio::test]
    async fn stage_two_finds_a_shift_beyond_the_aligners_reach() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&rec);
        let _guard = metrics::set_default_local_recorder(&rec);
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let mut fake = Fake::new(900, 250);
        fake.truth = Some((20_000, 1.0));
        let checker = job_hearing(&lib, &fake, Ears::hearing(Ok(spoken(20_000, 1.0))));
        checker.run_pass().await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("retimed"), "{}", r.reason);
        assert_eq!((r.offset_ms, r.ratio), (Some(20_000), Some(1.0)));
        // Stage 1, then the first stage-2 candidate fits: four requests.
        assert_eq!(fake.calls().len(), 4);
        assert_eq!(
            *checker.ears.streams.lock().unwrap(),
            [Some(1)],
            "the gate's stream"
        );
        assert!(r.scores_json.unwrap().contains("\"stage\":2"));
        assert_eq!(counts(&snap), [("retimed,offset,2".to_owned(), 1)]);
    }

    #[tokio::test]
    async fn a_timing_that_drops_a_probe_keeps_the_others_in_their_places() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let mut fake = Fake::new(900, 250);
        // The first cue would start before the file at this timing.
        fake.truth = Some((-20_000, 1.0));
        job_hearing(&lib, &fake, Ears::hearing(Ok(spoken(-20_000, 1.0))))
            .run_pass()
            .await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("retimed"), "{}", r.reason);
        assert_eq!(r.offset_ms, Some(-20_000));
        let json: serde_json::Value = serde_json::from_str(&r.scores_json.unwrap()).unwrap();
        let probes = json["stages"][1]["probes"].as_array().unwrap();
        assert_eq!(probes.len(), 12, "one score per probe, in probe order");
        assert!(probes[0].is_null(), "{probes:?}");
        assert!(
            probes[1..].iter().all(|p| p.as_f64() == Some(0.9)),
            "{probes:?}"
        );
    }

    #[tokio::test]
    async fn cues_all_past_the_end_still_get_the_wide_search() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let mut item = lib.store.get(1).await.unwrap();
        item.probe.duration_ms = Some(500);
        lib.store.put(item).await.unwrap();
        let fake = Fake::new(900, 250);
        let checker = job(&lib, &fake);
        checker.run_pass().await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("unknown"));
        assert!(
            r.reason.starts_with("inconclusive no_candidates"),
            "{}",
            r.reason
        );
        assert!(fake.calls().is_empty(), "nothing inside the file to send");
        assert_eq!(
            checker.ears.streams.lock().unwrap().len(),
            1,
            "stage 2 listened"
        );
    }

    #[tokio::test]
    async fn stage_two_finds_a_speed_ratio() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let ratio = 25.0 / 23.976;
        let mut fake = Fake::new(900, 250);
        fake.truth = Some((1_500, ratio));
        job_hearing(&lib, &fake, Ears::hearing(Ok(spoken(1_500, ratio))))
            .run_pass()
            .await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("retimed"), "{}", r.reason);
        assert_eq!(r.ratio, Some(ratio));
        assert!(
            (r.offset_ms.unwrap() - 1_500).abs() <= 100,
            "{:?}",
            r.offset_ms
        );
    }

    #[tokio::test]
    async fn a_track_that_fits_nowhere_is_wrong_after_at_most_eight_requests() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(270, 250);
        job_hearing(&lib, &fake, Ears::hearing(Ok(spoken(30_000, 1.0))))
            .run_pass()
            .await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("wrong_video"), "{}", r.reason);
        assert_eq!(fake.calls().len(), 2 + 2 * MAX_STAGE2);
    }

    #[tokio::test]
    async fn no_speech_match_is_inconclusive_not_wrong() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(270, 250);
        job(&lib, &fake).run_pass().await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("unknown"));
        assert!(
            r.reason.starts_with("inconclusive no_candidates"),
            "{}",
            r.reason
        );
        assert_eq!(fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn a_failed_decode_is_inconclusive_and_names_the_error() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(270, 250);
        let ears = Ears::hearing(Err("ffmpeg exit status: 1: no audio".into()));
        job_hearing(&lib, &fake, ears).run_pass().await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("unknown"));
        assert!(
            r.reason
                .starts_with("inconclusive coarse_error:ffmpeg exit status: 1: no audio"),
            "{}",
            r.reason
        );
        assert_eq!(fake.calls().len(), 2, "no stage-2 request");
    }

    #[tokio::test]
    async fn gate_outcomes_are_final_and_cost_no_request() {
        let text = srt();
        let short = "1\n00:00:01,000 --> 00:00:04,000\nI think this is the only line\n\n";
        let lib = library(&[
            (1, "anime.mkv", MediaKind::Episode, "jpn", Some(&text)),
            (2, "short.mkv", MediaKind::Movie, "eng", Some(short)),
        ])
        .await;
        let fake = Fake::new(900, 250);
        let sum = job(&lib, &fake).run_pass().await;
        assert_eq!(sum.verdicts.get("unknown"), Some(&2));
        assert!(fake.calls().is_empty());
        assert_eq!(row(&lib, 1).await.reason, "language_mismatch:en/ja");
        assert_eq!(row(&lib, 2).await.reason, "too_few_lines:1");
    }

    #[tokio::test]
    async fn aligner_failures_wait_an_hour_then_end_as_unknown_after_three() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(900, 250);
        fake.fail.store(10, Ordering::SeqCst);
        let sum = job(&lib, &fake).run_pass().await;
        assert_eq!(sum.retries, 1);
        let r = row(&lib, 1).await;
        assert_eq!((r.verdict.as_deref(), r.attempts), (None, 1));
        assert!(r.retry_at.unwrap() >= now_secs() + RETRY_AFTER_SECS - 5);
        assert_eq!(r.reason, "request: connection refused");

        // Not due yet: no request.
        job(&lib, &fake).run_pass().await;
        assert_eq!(fake.calls().len(), 1);

        for attempts in [2, 3] {
            let r = row(&lib, 1).await;
            lib.store
                .put_subtitle_check(SubtitleCheckRow {
                    retry_at: Some(0),
                    ..r
                })
                .await
                .unwrap();
            job(&lib, &fake).run_pass().await;
            let r = row(&lib, 1).await;
            assert_eq!(r.attempts, if attempts < 3 { attempts } else { 0 });
        }
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("unknown"));
        assert_eq!(r.reason, "aligner_error:request: connection refused");
    }

    #[tokio::test]
    async fn probes_are_sent_from_their_own_start_so_the_offset_is_theirs() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(900, 250);
        job(&lib, &fake).run_pass().await;
        let starts = fake.starts.lock().unwrap().clone();
        // Every cue starts one second into its minute: no start padding.
        assert!(starts.iter().all(|s| s % 60_000 == 1_000), "{starts:?}");
    }

    #[tokio::test]
    async fn a_failed_decoy_request_keeps_the_probe_scores() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let mut fake = Fake::new(900, 250);
        fake.fail_call = Some(1);
        job(&lib, &fake).run_pass().await;
        let r = row(&lib, 1).await;
        assert_eq!((r.verdict.as_deref(), r.attempts), (None, 1));
        let scores = r.scores_json.unwrap();
        assert!(scores.contains("0.9"), "{scores}");
    }

    #[tokio::test]
    async fn no_track_starts_while_a_transcode_waits() {
        let text = srt();
        // A gate outcome needs no aligner, and still waits for the queue.
        let lib = library(&[(1, "anime.mkv", MediaKind::Episode, "jpn", Some(&text))]).await;
        let fake = Fake::new(900, 250);
        let queued = Queued {
            busy: AtomicUsize::new(3),
        };
        let checker = Checker {
            store: lib.store.clone(),
            aligner: &fake,
            queue: &queued,
            ears: Ears::deaf(),
            bg_io: Arc::new(Semaphore::new(1)),
            cache: None,
            thresholds: Thresholds::default(),
            busy_poll: Duration::from_millis(1),
            health_poll: Duration::from_millis(1),
        };
        checker.run_pass().await;
        assert_eq!(
            queued.busy.load(Ordering::SeqCst),
            0,
            "waited out the queue"
        );
        assert_eq!(row(&lib, 1).await.verdict.as_deref(), Some("unknown"));
    }

    #[tokio::test]
    async fn a_song_queued_during_the_other_waits_still_goes_first() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(900, 250);
        // Idle at the first look, then a song arrives before the request.
        fake.busy_skip.store(1, Ordering::SeqCst);
        fake.busy.store(2, Ordering::SeqCst);
        job(&lib, &fake).run_pass().await;
        assert_eq!(
            *fake.busy_at_send.lock().unwrap(),
            [0, 0],
            "no request went out while a song was queued"
        );
        assert_eq!(fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn karaoke_goes_first() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(900, 250);
        fake.busy.store(3, Ordering::SeqCst);
        job(&lib, &fake).run_pass().await;
        assert_eq!(
            fake.busy.load(Ordering::SeqCst),
            0,
            "waited out every busy look"
        );
        assert_eq!(fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn films_go_before_episodes() {
        let text = srt();
        let lib = library(&[
            (1, "episode.mkv", MediaKind::Episode, "eng", Some(&text)),
            (2, "film.mkv", MediaKind::Movie, "eng", Some(&text)),
            (3, "song.flac", MediaKind::Audio, "eng", Some(&text)),
        ])
        .await;
        let fake = Fake::new(900, 250);
        job(&lib, &fake).run_pass().await;
        let order: Vec<String> = fake.calls().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            order,
            ["film.mkv", "film.mkv", "episode.mkv", "episode.mkv"]
        );
    }

    #[test]
    fn source_keys_are_stable_and_stage_labels_distinct() {
        let mut c = SubtitleCandidate {
            index: 4,
            language: None,
            codec: Some("subrip".into()),
            hearing_impaired: false,
            is_default: false,
            forced: false,
            signs_only: false,
            sidecar: None,
        };
        assert_eq!(source_key(&c), "stream:4");
        c.sidecar = Some("/m/a/film.en.srt".into());
        let k = source_key(&c);
        c.index = 9;
        c.sidecar = Some("/elsewhere/film.en.srt".into());
        assert_eq!(source_key(&c), k, "the file name, not the index or folder");
        assert_eq!(k, "sidecar:58963e67225fbd9d");
        let labels: std::collections::HashSet<_> = Stage::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), Stage::ALL.len());
    }
}
