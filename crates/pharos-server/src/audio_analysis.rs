//! Tempo and energy for every song (spec 024's tempo and energy mix rules;
//! ADR-0049).
//!
//! # Shape
//!
//! A slow, self-terminating background sweep in the mould of
//! [`crate::integrity_backfill`]. Each pass lists the library, picks the songs
//! with no current analysis, and analyses them one at a time on the libav
//! worker pool behind a metered `bg_io` permit, so it stands down whenever
//! someone is watching or listening (V34).
//!
//! # Why it is cheap
//!
//! - **Once per file.** A result is stored against the file signature it read
//!   and is read back only while the scan still sees that signature (V385). A
//!   pass over an analysed library decodes nothing; a re-probe keeps results,
//!   because `put` never writes these columns; only a changed file, or a new
//!   [`ANALYSIS_VERSION`], is redone.
//! - **The tag first.** The worker reads the BPM tag before decoding, and a
//!   tagged song skips the tempo estimator.
//! - **A window, not the song.** 30 s from a third of the way in, downmixed to
//!   mono and resampled to 11 kHz as it is decoded (`audio_features`).
//! - **One at a time, at background priority.** Sequential, so it holds one
//!   pool worker at most and leaves the rest to on-demand work, and every
//!   decode waits on the shared background-I/O gate the regulator parks
//!   during playback.
//! - **A failure is a result.** A file the decoder cannot read is stored as
//!   "found nothing", so it is not retried every pass (the V134 shape) until
//!   the file changes. Only a worker crash or timeout, which says nothing
//!   about the file, is left to retry.
//!
//! # Signals
//!
//! ```text
//! pharos_audio_analysis_total{outcome,tempo}  outcome: analysed | nothing | undecodable | error | missing | unscanned
//!                                             tempo:   tag | analysis | none
//! pharos_audio_analysis_seconds               per-song wall time, decode included
//! pharos_audio_analysis_pending               songs left at the start of a pass
//! ```
//!
//! Every song leaves one log line carrying its inputs (path, window) and its
//! verdict with the reason: the tempo and where it came from, the estimator's
//! confidence, the energy, or the decoder's own error.

use crate::bg_io::BgPermit;
use pharos_core::{AudioAnalysis, MediaItem, MediaKind, MediaStore, TempoSource};
use pharos_transcode::audio_features::{window_for, AudioFeatures, ANALYSIS_VERSION};
use pharos_transcode::protocol::WorkerError;
use pharos_transcode::worker::{LibavWorkerPool, PoolError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Delay before the first pass, so boot I/O settles.
const WARMUP: Duration = Duration::from_secs(180);

/// Interval between passes. A pass over an analysed library is one list and
/// no decodes, so this only governs how soon new songs are analysed.
const PASS_INTERVAL: Duration = Duration::from_secs(3600);

/// The decode the sweep needs. The pool in production; a fake in tests.
pub trait Analyser: Send + Sync {
    fn analyse(
        &self,
        path: PathBuf,
        start_ms: u64,
        dur_ms: u64,
    ) -> impl std::future::Future<Output = Result<AudioFeatures, PoolError>> + Send;
}

impl Analyser for LibavWorkerPool {
    async fn analyse(
        &self,
        path: PathBuf,
        start_ms: u64,
        dur_ms: u64,
    ) -> Result<AudioFeatures, PoolError> {
        self.audio_features(path, start_ms, dur_ms).await
    }
}

/// How one song's analysis ended: the `outcome` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A tempo or an energy was found and stored.
    Analysed,
    /// Decoded, but nothing to report (silence, too short); stored, so it is
    /// not decoded again.
    Nothing,
    /// The decoder refused the file; stored as nothing found.
    Undecodable,
    /// The worker died or timed out; not stored, retried next pass.
    Error,
    /// The file could not be stat'd; not stored. The scanner removes rows for
    /// files that are gone.
    Missing,
    /// The file on disk differs from what the last scan saw; not decoded until
    /// the scanner catches up.
    Unscanned,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Analysed => "analysed",
            Outcome::Nothing => "nothing",
            Outcome::Undecodable => "undecodable",
            Outcome::Error => "error",
            Outcome::Missing => "missing",
            Outcome::Unscanned => "unscanned",
        }
    }
}

/// The `tempo` label.
/// The `tempo` label when no tempo is known.
pub const NO_TEMPO: &str = "none";

/// The `tempo` label: `TempoSource::label()`, or [`NO_TEMPO`].
pub fn tempo_label(a: Option<&AudioAnalysis>) -> &'static str {
    a.and_then(|a| a.tempo_source)
        .map_or(NO_TEMPO, TempoSource::label)
}

/// Whether `item` is a song this sweep should analyse now.
pub fn needs_analysis(item: &MediaItem) -> bool {
    item.kind == MediaKind::Audio
        && item.origin().local().is_some()
        && match item.probe.audio_analysis {
            None => true,
            Some(a) => a.version < ANALYSIS_VERSION,
        }
}

/// What to store for a worker result, and how it ended. `None` = store
/// nothing (retry next pass).
pub fn verdict(result: &Result<AudioFeatures, PoolError>) -> (Option<AudioAnalysis>, Outcome) {
    match result {
        Ok(f) => {
            let a = AudioAnalysis::from_readings(f.bpm(), f.energy, ANALYSIS_VERSION);
            let outcome = if a.bpm_mille.is_some() || a.energy_mille.is_some() {
                Outcome::Analysed
            } else {
                Outcome::Nothing
            };
            (Some(a), outcome)
        }
        // The file's fault: remember it.
        Err(PoolError::Op(
            WorkerError::BadInput(_)
            | WorkerError::DecodeFailed(_)
            | WorkerError::UnsupportedCodec(_),
        )) => (
            Some(AudioAnalysis {
                version: ANALYSIS_VERSION,
                ..AudioAnalysis::default()
            }),
            Outcome::Undecodable,
        ),
        // Anything else says nothing about the file (libav init, resampler,
        // a dead or hung worker): retry next pass.
        Err(_) => (None, Outcome::Error),
    }
}

/// Spawn the sweep. Fire-and-forget: a failure aborts only this sweep
/// (logged), never the server.
pub fn spawn<S>(store: S, bg_leader: Arc<AtomicBool>, bg_io: Arc<Semaphore>, pool: LibavWorkerPool)
where
    S: MediaStore + 'static,
{
    tracing::info!(
        version = ANALYSIS_VERSION,
        "audio analysis: spawning tempo and energy sweep"
    );
    tokio::spawn(async move {
        tokio::time::sleep(WARMUP).await;
        // T85 — one replica sweeps; a second would decode the same songs again.
        while !bg_leader.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        loop {
            match store.list().await {
                Ok(items) => {
                    run_pass(&store, &pool, &bg_io, &items).await;
                }
                Err(e) => tracing::warn!(error = %e, "audio analysis: item list failed"),
            }
            tokio::time::sleep(PASS_INTERVAL).await;
        }
    });
}

/// Totals for one pass, for its summary line and the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassSummary {
    pub pending: usize,
    pub analysed: usize,
    pub nothing: usize,
    pub undecodable: usize,
    pub errors: usize,
    pub missing: usize,
    pub unscanned: usize,
}

/// Analyse every song in `items` that needs it, one at a time.
pub async fn run_pass<S: MediaStore, A: Analyser>(
    store: &S,
    analyser: &A,
    bg_io: &Arc<Semaphore>,
    items: &[MediaItem],
) -> PassSummary {
    let pending: Vec<&MediaItem> = items.iter().filter(|i| needs_analysis(i)).collect();
    let mut sum = PassSummary {
        pending: pending.len(),
        ..PassSummary::default()
    };
    metrics::gauge!("pharos_audio_analysis_pending").set(pending.len() as f64);
    if pending.is_empty() {
        return sum;
    }
    tracing::info!(pending = pending.len(), "audio analysis: pass starting");

    for it in pending {
        let started = Instant::now();
        let Some(sig) = signature(&it.path).await else {
            sum.missing += 1;
            metrics::counter!(
                "pharos_audio_analysis_total",
                "outcome" => Outcome::Missing.label(),
                "tempo" => NO_TEMPO,
            )
            .increment(1);
            tracing::warn!(
                item = %it.id,
                path = %it.path.display(),
                "audio analysis: file not found; skipped until the scanner catches up",
            );
            continue;
        };
        // A file changed since the last scan: its result would be stored
        // against a signature the row does not carry yet, read back as
        // unanalysed, and decoded again every pass. Leave it to the scanner,
        // which stamps the new signature (and re-probes) first.
        match store.scan_state(it.id).await {
            Ok(Some(st)) if st.file_mtime != 0 && (st.file_mtime, st.file_size) != sig => {
                sum.unscanned += 1;
                metrics::counter!(
                    "pharos_audio_analysis_total",
                    "outcome" => Outcome::Unscanned.label(),
                    "tempo" => NO_TEMPO,
                )
                .increment(1);
                tracing::debug!(
                    item = %it.id,
                    path = %it.path.display(),
                    scanned_mtime = st.file_mtime,
                    scanned_size = st.file_size,
                    disk_mtime = sig.0,
                    disk_size = sig.1,
                    "audio analysis: file changed since the last scan; waiting for the scanner",
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(item = %it.id, error = %e, "audio analysis: scan state read failed");
            }
        }
        let (start_ms, dur_ms) = window_for(it.probe.duration_ms);
        // Hold the gate for the decode only.
        let result = {
            let _permit = BgPermit::acquire(bg_io).await;
            analyser.analyse(it.path.clone(), start_ms, dur_ms).await
        };
        let elapsed = started.elapsed();
        let (stored, outcome) = verdict(&result);

        // A store failure is logged and counted as an error: the song is
        // simply picked up again next pass.
        let outcome = match stored {
            Some(a) => match store.put_audio_analysis(it.id, a, sig).await {
                Ok(()) => outcome,
                Err(e) => {
                    tracing::warn!(item = %it.id, error = %e, "audio analysis: store write failed");
                    Outcome::Error
                }
            },
            None => outcome,
        };
        let tempo = tempo_label(stored.as_ref());
        metrics::counter!(
            "pharos_audio_analysis_total",
            "outcome" => outcome.label(),
            "tempo" => tempo,
        )
        .increment(1);
        metrics::histogram!("pharos_audio_analysis_seconds").record(elapsed.as_secs_f64());

        match (&result, outcome) {
            (Ok(f), Outcome::Analysed | Outcome::Nothing) => {
                if outcome == Outcome::Analysed {
                    sum.analysed += 1;
                } else {
                    sum.nothing += 1;
                }
                tracing::debug!(
                    item = %it.id,
                    path = %it.path.display(),
                    outcome = outcome.label(),
                    start_ms,
                    dur_ms,
                    analysed_s = f.analysed_seconds,
                    tag_bpm = f.tag_bpm,
                    analysed_bpm = f.analysed_bpm,
                    tempo_confidence = f.tempo_confidence,
                    tempo,
                    energy = f.energy,
                    elapsed_s = elapsed.as_secs_f64(),
                    "audio analysis: song analysed",
                );
            }
            _ => {
                if outcome == Outcome::Undecodable {
                    sum.undecodable += 1;
                } else {
                    sum.errors += 1;
                }
                let error = match &result {
                    Err(e) => e.to_string(),
                    Ok(_) => "store write failed".to_owned(),
                };
                tracing::warn!(
                    item = %it.id,
                    path = %it.path.display(),
                    outcome = outcome.label(),
                    start_ms,
                    dur_ms,
                    error = %error,
                    elapsed_s = elapsed.as_secs_f64(),
                    "audio analysis: song not analysed",
                );
            }
        }
    }

    tracing::info!(
        pending = sum.pending,
        analysed = sum.analysed,
        nothing = sum.nothing,
        undecodable = sum.undecodable,
        errors = sum.errors,
        missing = sum.missing,
        unscanned = sum.unscanned,
        "audio analysis: pass complete",
    );
    sum
}

/// The file signature the scanner records: whole-second mtime and size.
async fn signature(path: &std::path::Path) -> Option<(i64, u64)> {
    let m = tokio::fs::metadata(path).await.ok()?;
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    Some((mtime, m.len()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::MediaProbe;
    use pharos_store_sqlx::sqlite::SqliteStore;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Scripted worker: a result per file name, and a record of every call.
    #[derive(Default)]
    struct Fake {
        script: HashMap<String, Result<AudioFeatures, String>>,
        calls: Mutex<Vec<(String, u64, u64)>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<(String, u64, u64)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Analyser for Fake {
        async fn analyse(
            &self,
            path: PathBuf,
            start_ms: u64,
            dur_ms: u64,
        ) -> Result<AudioFeatures, PoolError> {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            self.calls
                .lock()
                .unwrap()
                .push((name.clone(), start_ms, dur_ms));
            match self.script.get(&name) {
                Some(Ok(f)) => Ok(*f),
                Some(Err(e)) if e == "dead" => Err(PoolError::Dead(e.clone())),
                Some(Err(e)) if e == "resampler" => {
                    Err(PoolError::Op(WorkerError::Other(e.clone())))
                }
                Some(Err(e)) => Err(PoolError::Op(WorkerError::BadInput(e.clone()))),
                None => panic!("unscripted file {name}"),
            }
        }
    }

    fn tagged() -> AudioFeatures {
        AudioFeatures {
            tag_bpm: Some(128.0),
            energy: Some(0.8),
            analysed_seconds: 30.0,
            ..AudioFeatures::default()
        }
    }

    fn untagged() -> AudioFeatures {
        AudioFeatures {
            analysed_bpm: Some(92.5),
            tempo_confidence: 0.4,
            energy: Some(0.31),
            analysed_seconds: 30.0,
            ..AudioFeatures::default()
        }
    }

    struct Lib {
        _dir: TempDir,
        store: SqliteStore,
        items: Vec<MediaItem>,
    }

    /// A library of real (empty) files so the sweep can stat them.
    async fn library(files: &[(u64, &str, MediaKind)]) -> Lib {
        let dir = TempDir::new().unwrap();
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        for &(id, name, kind) in files {
            let path = dir.path().join(name);
            if name != "gone.flac" {
                std::fs::write(&path, b"x").unwrap();
            }
            store
                .put(MediaItem {
                    id,
                    path,
                    title: name.into(),
                    kind,
                    probe: MediaProbe {
                        duration_ms: Some(240_000),
                        ..MediaProbe::default()
                    },
                    ..MediaItem::default()
                })
                .await
                .unwrap();
        }
        let items = store.list().await.unwrap();
        Lib {
            _dir: dir,
            store,
            items,
        }
    }

    fn gate() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(1))
    }

    #[tokio::test]
    async fn a_pass_stores_tag_and_analysed_tempo_and_skips_what_is_not_a_song() {
        let lib = library(&[
            (1, "tagged.mp3", MediaKind::Audio),
            (2, "plain.flac", MediaKind::Audio),
            (3, "film.mkv", MediaKind::Movie),
        ])
        .await;
        let fake = Fake {
            script: HashMap::from([
                ("tagged.mp3".into(), Ok(tagged())),
                ("plain.flac".into(), Ok(untagged())),
            ]),
            ..Fake::default()
        };
        let sum = run_pass(&lib.store, &fake, &gate(), &lib.items).await;
        assert_eq!(sum.pending, 2, "the film is not a song");
        assert_eq!(sum.analysed, 2);

        // The window sits a third into a four-minute song.
        assert!(fake.calls().iter().all(|c| c.1 == 80_000 && c.2 == 30_000));

        let t = lib.store.get(1).await.unwrap().probe;
        assert_eq!(t.bpm(), Some(128.0));
        assert_eq!(t.tempo_source(), Some(TempoSource::Tag));
        assert_eq!(t.energy(), Some(0.8));
        let p = lib.store.get(2).await.unwrap().probe;
        assert_eq!(p.bpm(), Some(92.5));
        assert_eq!(p.tempo_source(), Some(TempoSource::Analysis));
        assert_eq!(p.energy(), Some(0.31));
        assert_eq!(lib.store.get(3).await.unwrap().probe.audio_analysis, None);
    }

    /// V385 — a song is decoded once. The second pass over the same library,
    /// read back from the store, decodes nothing; a re-probe (`put`) keeps the
    /// result; a changed file is redone.
    #[tokio::test]
    async fn a_song_is_decoded_once_until_its_file_changes() {
        let lib = library(&[(1, "tagged.mp3", MediaKind::Audio)]).await;
        let fake = Fake {
            script: HashMap::from([("tagged.mp3".into(), Ok(tagged()))]),
            ..Fake::default()
        };
        run_pass(&lib.store, &fake, &gate(), &lib.items).await;
        assert_eq!(fake.calls().len(), 1);

        // A scan stamps the signature the sweep read: the result still holds.
        let path = lib.items[0].path.clone();
        let (mtime, size) = signature(&path).await.unwrap();
        let scan = lib.store.begin_scan(path.parent().unwrap()).await.unwrap();
        lib.store.mark_seen(1, scan, mtime, size).await.unwrap();

        // A re-probe writes the row again without the analysis.
        let mut reprobed = lib.store.get(1).await.unwrap();
        reprobed.probe.audio_analysis = None;
        lib.store.put(reprobed).await.unwrap();

        let items = lib.store.list().await.unwrap();
        assert!(
            items[0].probe.bpm().is_some(),
            "a re-probe keeps the analysis"
        );
        let sum = run_pass(&lib.store, &fake, &gate(), &items).await;
        assert_eq!(
            (sum.pending, fake.calls().len()),
            (0, 1),
            "nothing decoded again"
        );

        // The file changes; the scan stamps its new signature.
        std::fs::write(&path, b"a longer file").unwrap();
        let (mtime2, size2) = signature(&path).await.unwrap();
        lib.store.mark_seen(1, scan, mtime2, size2).await.unwrap();
        let items = lib.store.list().await.unwrap();
        assert_eq!(
            items[0].probe.audio_analysis, None,
            "a changed file reads unanalysed"
        );
        run_pass(&lib.store, &fake, &gate(), &items).await;
        assert_eq!(fake.calls().len(), 2, "and is redone");
    }

    /// The V134 shape: a file the decoder refuses is stored as "found nothing",
    /// so it is not decoded again every pass. A worker crash says nothing
    /// about the file and is retried.
    #[tokio::test]
    async fn an_undecodable_file_is_remembered_and_a_crash_is_retried() {
        let lib = library(&[
            (1, "broken.mp3", MediaKind::Audio),
            (2, "crash.flac", MediaKind::Audio),
            (3, "gone.flac", MediaKind::Audio),
            (4, "resampler.ogg", MediaKind::Audio),
        ])
        .await;
        let fake = Fake {
            script: HashMap::from([
                (
                    "broken.mp3".into(),
                    Err("bad input: no audio stream".into()),
                ),
                ("crash.flac".into(), Err("dead".into())),
                ("resampler.ogg".into(), Err("resampler".into())),
            ]),
            ..Fake::default()
        };
        let sum = run_pass(&lib.store, &fake, &gate(), &lib.items).await;
        assert_eq!((sum.undecodable, sum.errors, sum.missing), (1, 2, 1));
        assert_eq!(
            fake.calls().len(),
            3,
            "a missing file is never offered to the worker"
        );

        let broken = lib
            .store
            .get(1)
            .await
            .unwrap()
            .probe
            .audio_analysis
            .unwrap();
        assert_eq!((broken.bpm(), broken.energy()), (None, None));
        assert_eq!(broken.version, ANALYSIS_VERSION);
        assert_eq!(lib.store.get(2).await.unwrap().probe.audio_analysis, None);
        assert_eq!(
            lib.store.get(4).await.unwrap().probe.audio_analysis,
            None,
            "a worker-side failure is not a verdict on the file"
        );

        let items = lib.store.list().await.unwrap();
        run_pass(&lib.store, &fake, &gate(), &items).await;
        let names: Vec<String> = fake.calls().into_iter().map(|c| c.0).collect();
        assert_eq!(
            names,
            [
                "broken.mp3",
                "crash.flac",
                "resampler.ogg",
                "crash.flac",
                "resampler.ogg"
            ]
        );
    }

    #[test]
    fn an_older_analyser_version_is_redone() {
        let mut item = MediaItem {
            kind: MediaKind::Audio,
            path: "/music/a.flac".into(),
            ..MediaItem::default()
        };
        assert!(needs_analysis(&item));
        item.probe.audio_analysis = Some(AudioAnalysis {
            version: ANALYSIS_VERSION,
            ..AudioAnalysis::default()
        });
        assert!(!needs_analysis(&item));
        item.probe.audio_analysis = Some(AudioAnalysis {
            version: ANALYSIS_VERSION - 1,
            ..AudioAnalysis::default()
        });
        assert!(needs_analysis(&item));
        item.path = "ytdlp://youtube/abc".into();
        assert!(!needs_analysis(&item), "a remote origin is not a file");
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let all = [
            Outcome::Analysed,
            Outcome::Nothing,
            Outcome::Undecodable,
            Outcome::Error,
            Outcome::Missing,
            Outcome::Unscanned,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn tempo_labels_are_distinct() {
        let tag = AudioAnalysis {
            tempo_source: Some(TempoSource::Tag),
            ..AudioAnalysis::default()
        };
        let est = AudioAnalysis {
            tempo_source: Some(TempoSource::Analysis),
            ..AudioAnalysis::default()
        };
        let labels: std::collections::HashSet<_> = [
            tempo_label(Some(&tag)),
            tempo_label(Some(&est)),
            tempo_label(None),
            tempo_label(Some(&AudioAnalysis::default())),
        ]
        .into_iter()
        .collect();
        assert_eq!(labels.len(), 3, "{labels:?}");
        assert_eq!(tempo_label(None), NO_TEMPO);
    }

    /// A file changed on disk since the last scan is not decoded: its result
    /// could not be read back until the scanner stamps the new signature, so
    /// decoding it would repeat every pass.
    #[tokio::test]
    async fn a_file_changed_since_the_scan_waits_for_the_scanner() {
        let lib = library(&[(1, "tagged.mp3", MediaKind::Audio)]).await;
        let path = lib.items[0].path.clone();
        let (mtime, size) = signature(&path).await.unwrap();
        let scan = lib.store.begin_scan(path.parent().unwrap()).await.unwrap();
        lib.store
            .mark_seen(1, scan, mtime - 60, size)
            .await
            .unwrap();
        let fake = Fake {
            script: HashMap::from([("tagged.mp3".into(), Ok(tagged()))]),
            ..Fake::default()
        };
        let items = lib.store.list().await.unwrap();
        let sum = run_pass(&lib.store, &fake, &gate(), &items).await;
        assert_eq!((sum.unscanned, fake.calls().len()), (1, 0));

        // The scanner catches up: now it is analysed.
        lib.store.mark_seen(1, scan, mtime, size).await.unwrap();
        let items = lib.store.list().await.unwrap();
        let sum = run_pass(&lib.store, &fake, &gate(), &items).await;
        assert_eq!((sum.analysed, fake.calls().len()), (1, 1));
    }

    /// ODD — the failure path records as much as the success path: every
    /// song leaves one `pharos_audio_analysis_total` sample.
    #[tokio::test]
    async fn every_song_leaves_a_metric_sample() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let lib = library(&[
            (1, "tagged.mp3", MediaKind::Audio),
            (2, "broken.mp3", MediaKind::Audio),
        ])
        .await;
        let fake = Fake {
            script: HashMap::from([
                ("tagged.mp3".into(), Ok(tagged())),
                ("broken.mp3".into(), Err("bad input".into())),
            ]),
            ..Fake::default()
        };
        run_pass(&lib.store, &fake, &gate(), &lib.items).await;

        let mut seen = Vec::new();
        for (ck, _, _, v) in snapshotter.snapshot().into_vec() {
            if ck.key().name() != "pharos_audio_analysis_total" {
                continue;
            }
            let label = |n: &str| {
                ck.key()
                    .labels()
                    .find(|l| l.key() == n)
                    .map(|l| l.value().to_owned())
                    .unwrap()
            };
            assert!(matches!(v, DebugValue::Counter(1)), "{v:?}");
            seen.push((label("outcome"), label("tempo")));
        }
        seen.sort();
        assert_eq!(
            seen,
            [
                ("analysed".to_owned(), "tag".to_owned()),
                ("undecodable".to_owned(), "none".to_owned()),
            ]
        );
    }
}
