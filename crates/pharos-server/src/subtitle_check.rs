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
//! - **Stage 1 only.** The wide search (stage 2) is slice 3: until then a
//!   track that does not fit is `unknown`, `not_searched`, never `wrong_video`.
//! - **Log only.** Nothing reads the rows to change what is served until
//!   `[subtitle_check].apply` and slice 5.
//!
//! # Signals
//!
//! ```text
//! pharos_subtitle_check_total{verdict,reason_class,stage}  stage: gate | 1
//! pharos_subtitle_check_aligner_seconds                    aligner time per check
//! pharos_subtitle_check_pending                            tracks left at the start of a pass
//! ```
//!
//! Every track leaves one `subtitle check` log line with its item, source,
//! languages, verdict, reason, every probe and decoy score and the aligner
//! time; a failure carries the same fields.

use crate::bg_io::BgPermit;
use crate::services::lyrics::align::{Aligned, Aligner};
use crate::subtitle_styling::{
    candidates_for, read_source, signature, SubtitleCandidate, TranscodeQueue,
};
use pharos_cache::SubtitleCache;
use pharos_core::{MediaItem, MediaKind, MediaStore, SubtitleCheckRow, SubtitleCheckStore};
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
pub const CHECK_VERSION: u32 = 1;
/// FR-007: each probe's window ends this much after the cue.
pub const PAD_MS: u64 = 2_000;
/// FR-015a: an aligner failure is retried no sooner than this (seconds), as
/// the karaoke queue's `RETRY_AFTER`.
pub const RETRY_AFTER_SECS: i64 = 3_600;
/// FR-015a: attempts before an aligner failure is stored as `unknown`.
pub const MAX_ATTEMPTS: u32 = 3;

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

pub trait CheckStore: MediaStore + SubtitleCheckStore {}
impl<S: MediaStore + SubtitleCheckStore> CheckStore for S {}

/// The `stage` label: where a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// FR-002 to FR-004: decided from the files, no aligner request.
    Gate,
    /// FR-007: the probes at their own times.
    One,
}

impl Stage {
    pub const ALL: [Stage; 2] = [Stage::Gate, Stage::One];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Gate => "gate",
            Stage::One => "1",
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
    let tracks = &item.probe.audio_tracks;
    tracks
        .iter()
        .find(|t| t.is_default)
        .or_else(|| tracks.first())
        .and_then(|t| t.language.as_deref())
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
pub struct Checker<S, A, Q> {
    pub store: S,
    pub aligner: A,
    pub queue: Q,
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
    aligner_s: f64,
}

impl Measured {
    fn scores_json(&self) -> Option<String> {
        let c = self.stage1.as_ref()?;
        let finite = |v: &[f32]| -> Vec<Option<f64>> {
            v.iter()
                .map(|s| {
                    s.is_finite()
                        .then(|| (f64::from(*s) * 1000.0).round() / 1000.0)
                })
                .collect()
        };
        Some(
            serde_json::json!({
                "stages": [{
                    "stage": 1,
                    "offset_ms": c.offset_ms,
                    "ratio": c.ratio,
                    "probes": finite(&c.probes),
                    "decoys": finite(&c.decoys),
                }]
            })
            .to_string(),
        )
    }
}

impl<S: CheckStore, A: Scorer, Q: TranscodeQueue> Checker<S, A, Q> {
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
        let (verdict, stage) = match self
            .judge(item, c, &script, audio_lang, &mut measured)
            .await
        {
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
                    Stage::One,
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

    /// The gate, then stage 1. `Err` is an aligner failure.
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
        self.wait_for_turn().await;
        let lines = |text: fn(&Probe) -> &str| -> Vec<(u64, u64, String)> {
            picked
                .iter()
                .map(|p| {
                    let (a, b) = window(p);
                    (a, b, text(p).to_owned())
                })
                .collect()
        };
        let asked = Instant::now();
        let probe_answer = self.aligner.score(&item.path, &lines(|p| &p.text)).await;
        measured.aligner_s = asked.elapsed().as_secs_f64();
        let probe_answer = match probe_answer {
            Ok(a) => a,
            Err(e) => {
                metrics::histogram!("pharos_subtitle_check_aligner_seconds")
                    .record(measured.aligner_s);
                return Err(e);
            }
        };
        // FR-016: one request at a time, and a song may have come in between.
        self.wait_for_turn().await;
        let decoy_asked = Instant::now();
        let decoy_answer = self.aligner.score(&item.path, &lines(|p| &p.decoy)).await;
        measured.aligner_s += decoy_asked.elapsed().as_secs_f64();
        metrics::histogram!("pharos_subtitle_check_aligner_seconds").record(measured.aligner_s);
        let decoy_answer = decoy_answer?;
        let cand = Candidate {
            offset_ms: probe_answer.offset_ms,
            ratio: 1.0,
            probes: scores(&probe_answer, picked.len()),
            decoys: scores(&decoy_answer, picked.len()),
        };
        let v = verdict::decide(&cand, None, &self.thresholds);
        measured.stage1 = Some(cand);
        Ok((v, Stage::One))
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
            aligner_s = measured.aligner_s,
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
        healthy: bool,
        calls: Mutex<Vec<(String, usize)>>,
        starts: Mutex<Vec<u64>>,
    }

    impl Fake {
        fn new(probe: u16, decoy: u16) -> Self {
            Self {
                probe,
                decoy,
                offset_ms: 0,
                fail: AtomicUsize::new(0),
                busy: AtomicUsize::new(0),
                healthy: true,
                calls: Mutex::default(),
                starts: Mutex::default(),
            }
        }
        fn calls(&self) -> Vec<(String, usize)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Scorer for &Fake {
        fn busy(&self) -> bool {
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
            if self
                .fail
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err("request: connection refused".into());
            }
            let s = if is_decoy { self.decoy } else { self.probe };
            Ok(Aligned {
                lines: vec![Vec::new(); lines.len()],
                offset_ms: self.offset_ms,
                scores: vec![Some(s); lines.len()],
            })
        }
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

    fn job<'a>(lib: &Lib, fake: &'a Fake) -> Checker<SqliteStore, &'a Fake, Idle> {
        Checker {
            store: lib.store.clone(),
            aligner: fake,
            queue: Idle,
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
    async fn without_the_wide_search_a_misfit_is_unknown_not_wrong() {
        let text = srt();
        let lib = library(&[(1, "film.mkv", MediaKind::Movie, "eng", Some(&text))]).await;
        let fake = Fake::new(270, 250);
        job(&lib, &fake).run_pass().await;
        let r = row(&lib, 1).await;
        assert_eq!(r.verdict.as_deref(), Some("unknown"));
        assert!(r.reason.starts_with("not_searched"), "{}", r.reason);
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
