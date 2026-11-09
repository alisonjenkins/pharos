//! Black bars baked into a film's or episode's picture, found once per file so
//! the client can crop to the picture.
//!
//! # Shape
//!
//! The same slow, self-terminating background sweep as
//! [`crate::audio_analysis`]. Each pass lists the library, picks the films and
//! episodes with no current check, and checks them one at a time on the libav
//! worker pool behind a metered `bg_io` permit, so it stands down whenever
//! someone is watching or listening (V34).
//!
//! - **Once per file.** A result is stored against the file signature it read
//!   and read back only while the scan still sees that signature; a re-probe
//!   keeps it because `put` never writes these columns. Only a changed file or
//!   a new [`PICTURE_CHECK_VERSION`] is redone.
//! - **Seven keyframes, not the file.** The worker seeks to 10%..70% of the
//!   duration (`pharos_transcode::picture_area`).
//! - **A failure is a result.** A file the decoder refuses, and a file whose
//!   frames were too dark to judge, are stored as "no area", so they are not
//!   decoded every pass. Only a worker crash or timeout, which says nothing
//!   about the file, is left to retry.
//! - **3D is skipped.** A packed stereo frame has bars of its own per eye.
//!
//! # Signals
//!
//! ```text
//! pharos_picture_check_total{outcome}  outcome: bars | none | undetermined | undecodable | error | missing | unscanned
//! pharos_picture_check_seconds         per-file wall time, decode included
//! pharos_picture_check_pending         files left at the start of a pass
//! ```
//!
//! Every file leaves one log line: path, frame size, usable frames, and the
//! verdict with the area (`1436x1080+242+0`) or the reason there is none.

use crate::bg_io::BgPermit;
use crate::subtitle_styling::signature;
use pharos_core::{MediaItem, MediaKind, MediaStore, PictureArea, PictureCheck};
use pharos_transcode::picture_area::{
    PictureAnalysis, PictureVerdict, MIN_USABLE_FRAMES, PICTURE_CHECK_VERSION,
};
use pharos_transcode::protocol::WorkerError;
use pharos_transcode::worker::{LibavWorkerPool, PoolError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Delay before the first pass, so boot I/O settles.
pub(crate) const WARMUP: Duration = Duration::from_secs(240);

/// Interval between passes. A pass over a checked library is one list and no
/// decodes, so this only governs how soon new files are checked.
pub(crate) const PASS_INTERVAL: Duration = Duration::from_secs(3600);

/// The decode the sweep needs. The pool in production; a fake in tests.
pub trait Checker: Send + Sync {
    fn check(
        &self,
        path: PathBuf,
        duration_ms: u64,
    ) -> impl std::future::Future<Output = Result<PictureAnalysis, PoolError>> + Send;
}

impl Checker for LibavWorkerPool {
    async fn check(&self, path: PathBuf, duration_ms: u64) -> Result<PictureAnalysis, PoolError> {
        self.picture_check(path, duration_ms).await
    }
}

/// How one file's check ended: the `outcome` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Bars found; the area is stored.
    Bars,
    /// Checked, and the picture fills the frame (or the box found was not
    /// plausibly a picture); stored as no area.
    None,
    /// Too few usable frames to tell; stored as no area.
    Undetermined,
    /// The decoder refused the file; stored as no area.
    Undecodable,
    /// The worker died or timed out; not stored, retried next pass.
    Error,
    /// The file could not be stat'd; not stored.
    Missing,
    /// The file on disk differs from what the last scan saw; waits for the
    /// scanner.
    Unscanned,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Bars => "bars",
            Outcome::None => "none",
            Outcome::Undetermined => "undetermined",
            Outcome::Undecodable => "undecodable",
            Outcome::Error => "error",
            Outcome::Missing => "missing",
            Outcome::Unscanned => "unscanned",
        }
    }
}

/// Whether `item` is a film or episode this sweep should check now.
pub fn needs_check(item: &MediaItem) -> bool {
    matches!(item.kind, MediaKind::Movie | MediaKind::Episode)
        && item.origin().local().is_some()
        && item.probe.video_3d_format.is_none()
        && match item.probe.picture_check {
            None => true,
            Some(c) => c.version < PICTURE_CHECK_VERSION,
        }
}

/// `1436x1080+242+0`, the ffmpeg crop geometry.
pub fn area_string(a: PictureArea) -> String {
    format!("{}x{}+{}+{}", a.width, a.height, a.x, a.y)
}

/// What to store for a worker result, and how it ended. `None` = store
/// nothing (retry next pass).
pub fn verdict(result: &Result<PictureAnalysis, PoolError>) -> (Option<PictureCheck>, Outcome) {
    let stored = |area| {
        Some(PictureCheck {
            version: PICTURE_CHECK_VERSION,
            area,
        })
    };
    match result {
        Ok(a) => match a.verdict {
            PictureVerdict::Bars(area) => (stored(Some(area)), Outcome::Bars),
            PictureVerdict::NoBars | PictureVerdict::Implausible => (stored(None), Outcome::None),
            PictureVerdict::Undetermined => (stored(None), Outcome::Undetermined),
        },
        // The file's fault: remember it.
        Err(PoolError::Op(
            WorkerError::BadInput(_)
            | WorkerError::DecodeFailed(_)
            | WorkerError::UnsupportedCodec(_),
        )) => (stored(None), Outcome::Undecodable),
        // Anything else says nothing about the file (libav init, a dead or
        // hung worker): retry next pass.
        Err(_) => (None, Outcome::Error),
    }
}

/// Why a verdict has no area, for the log line.
fn reason(a: &PictureAnalysis) -> &'static str {
    match a.verdict {
        PictureVerdict::Bars(_) => "bars",
        PictureVerdict::NoBars => "the picture fills the frame",
        PictureVerdict::Implausible => "the lit area is under 40% of the frame",
        PictureVerdict::Undetermined if a.sampled_frames == 0 => "no frame decoded",
        PictureVerdict::Undetermined => "too few frames bright enough to judge",
    }
}

fn record(outcome: Outcome) {
    metrics::counter!("pharos_picture_check_total", "outcome" => outcome.label()).increment(1);
}

/// Spawn the sweep. Fire-and-forget: a failure aborts only this sweep
/// (logged), never the server.
pub fn spawn<S>(store: S, bg_leader: Arc<AtomicBool>, bg_io: Arc<Semaphore>, pool: LibavWorkerPool)
where
    S: MediaStore + 'static,
{
    tracing::info!(
        version = PICTURE_CHECK_VERSION,
        min_usable_frames = MIN_USABLE_FRAMES,
        "picture check: spawning black-bar sweep"
    );
    tokio::spawn(async move {
        tokio::time::sleep(WARMUP).await;
        // T85 — one replica sweeps; a second would decode the same files again.
        while !bg_leader.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        loop {
            match store.list().await {
                Ok(items) => {
                    run_pass(&store, &pool, &bg_io, &items).await;
                }
                Err(e) => tracing::warn!(error = %e, "picture check: item list failed"),
            }
            tokio::time::sleep(PASS_INTERVAL).await;
        }
    });
}

/// Totals for one pass, for its summary line and the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassSummary {
    pub pending: usize,
    pub bars: usize,
    pub none: usize,
    pub undetermined: usize,
    pub undecodable: usize,
    pub errors: usize,
    pub missing: usize,
    pub unscanned: usize,
}

/// Check every film and episode in `items` that needs it, one at a time.
pub async fn run_pass<S: MediaStore, C: Checker>(
    store: &S,
    checker: &C,
    bg_io: &Arc<Semaphore>,
    items: &[MediaItem],
) -> PassSummary {
    let pending: Vec<&MediaItem> = items.iter().filter(|i| needs_check(i)).collect();
    let mut sum = PassSummary {
        pending: pending.len(),
        ..PassSummary::default()
    };
    metrics::gauge!("pharos_picture_check_pending").set(pending.len() as f64);
    if pending.is_empty() {
        return sum;
    }
    tracing::info!(pending = pending.len(), "picture check: pass starting");

    for it in pending {
        let started = Instant::now();
        let Some(sig) = signature(&it.path).await else {
            sum.missing += 1;
            record(Outcome::Missing);
            tracing::warn!(
                item = %it.id,
                path = %it.path.display(),
                "picture check: file not found; skipped until the scanner catches up",
            );
            continue;
        };
        // A file changed since the last scan would have its result stored
        // against a signature the row does not carry yet, read back as
        // unchecked and decoded again every pass: leave it to the scanner.
        match store.scan_state(it.id).await {
            Ok(Some(st)) if st.file_mtime != 0 && (st.file_mtime, st.file_size) != sig => {
                sum.unscanned += 1;
                record(Outcome::Unscanned);
                tracing::debug!(
                    item = %it.id,
                    path = %it.path.display(),
                    scanned_mtime = st.file_mtime,
                    scanned_size = st.file_size,
                    disk_mtime = sig.0,
                    disk_size = sig.1,
                    "picture check: file changed since the last scan; waiting for the scanner",
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(item = %it.id, error = %e, "picture check: scan state read failed");
            }
        }
        // Hold the gate for the decode only.
        let result = {
            let _permit = BgPermit::acquire(bg_io).await;
            checker
                .check(it.path.clone(), it.probe.duration_ms.unwrap_or(0))
                .await
        };
        let elapsed = started.elapsed();
        let (stored, outcome) = verdict(&result);

        // A store failure is logged and counted as an error: the file is
        // simply picked up again next pass.
        let outcome = match stored {
            Some(c) => match store.put_picture_check(it.id, c, sig).await {
                Ok(()) => outcome,
                Err(e) => {
                    tracing::warn!(item = %it.id, error = %e, "picture check: store write failed");
                    Outcome::Error
                }
            },
            None => outcome,
        };
        record(outcome);
        metrics::histogram!("pharos_picture_check_seconds").record(elapsed.as_secs_f64());

        match outcome {
            Outcome::Bars | Outcome::None | Outcome::Undetermined => {
                match outcome {
                    Outcome::Bars => sum.bars += 1,
                    Outcome::None => sum.none += 1,
                    _ => sum.undetermined += 1,
                }
                if let Ok(a) = &result {
                    let area = match a.verdict {
                        PictureVerdict::Bars(area) => area_string(area),
                        _ => "-".to_owned(),
                    };
                    tracing::info!(
                        item = %it.id,
                        path = %it.path.display(),
                        outcome = outcome.label(),
                        frame_width = a.width,
                        frame_height = a.height,
                        usable_frames = a.usable_frames,
                        sampled_frames = a.sampled_frames,
                        area = %area,
                        reason = reason(a),
                        elapsed_s = elapsed.as_secs_f64(),
                        "picture check: file checked",
                    );
                }
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
                    error = %error,
                    elapsed_s = elapsed.as_secs_f64(),
                    "picture check: file not checked",
                );
            }
        }
    }

    tracing::info!(
        pending = sum.pending,
        bars = sum.bars,
        none = sum.none,
        undetermined = sum.undetermined,
        undecodable = sum.undecodable,
        errors = sum.errors,
        missing = sum.missing,
        unscanned = sum.unscanned,
        "picture check: pass complete",
    );
    sum
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use pharos_core::{MediaProbe, Video3DFormat};
    use pharos_store_sqlx::sqlite::SqliteStore;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Scripted worker: a result per file name, and a record of every call.
    #[derive(Default)]
    struct Fake {
        script: HashMap<String, Result<PictureAnalysis, String>>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Checker for Fake {
        async fn check(&self, path: PathBuf, _: u64) -> Result<PictureAnalysis, PoolError> {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            self.calls.lock().unwrap().push(name.clone());
            match self.script.get(&name) {
                Some(Ok(a)) => Ok(*a),
                Some(Err(e)) if e == "dead" => Err(PoolError::Dead(e.clone())),
                Some(Err(e)) => Err(PoolError::Op(WorkerError::BadInput(e.clone()))),
                None => panic!("unscripted file {name}"),
            }
        }
    }

    const BARS: PictureArea = PictureArea {
        x: 242,
        y: 0,
        width: 1436,
        height: 1080,
    };

    fn analysis(verdict: PictureVerdict, usable: u32) -> PictureAnalysis {
        PictureAnalysis {
            verdict,
            width: 1920,
            height: 1080,
            usable_frames: usable,
            sampled_frames: 7,
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
            if name != "gone.mkv" {
                std::fs::write(&path, b"x").unwrap();
            }
            let video_3d_format = (name == "sbs.mkv").then_some(Video3DFormat::HalfSideBySide);
            store
                .put(MediaItem {
                    id,
                    path,
                    title: name.into(),
                    kind,
                    probe: MediaProbe {
                        duration_ms: Some(2_400_000),
                        video_3d_format,
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
    async fn a_pass_stores_bars_and_no_bars_and_skips_songs_and_3d() {
        let lib = library(&[
            (1, "bewitched.mkv", MediaKind::Episode),
            (2, "film.mkv", MediaKind::Movie),
            (3, "song.flac", MediaKind::Audio),
            (4, "sbs.mkv", MediaKind::Movie),
            (5, "dark.mkv", MediaKind::Movie),
        ])
        .await;
        let fake = Fake {
            script: HashMap::from([
                (
                    "bewitched.mkv".into(),
                    Ok(analysis(PictureVerdict::Bars(BARS), 7)),
                ),
                ("film.mkv".into(), Ok(analysis(PictureVerdict::NoBars, 7))),
                (
                    "dark.mkv".into(),
                    Ok(analysis(PictureVerdict::Undetermined, 1)),
                ),
            ]),
            ..Fake::default()
        };
        let sum = run_pass(&lib.store, &fake, &gate(), &lib.items).await;
        assert_eq!(sum.pending, 3, "neither the song nor the 3D film");
        assert_eq!((sum.bars, sum.none, sum.undetermined), (1, 1, 1));
        assert_eq!(fake.calls(), ["bewitched.mkv", "film.mkv", "dark.mkv"]);

        let got = lib.store.get(1).await.unwrap().probe;
        assert_eq!(got.picture_area(), Some(BARS));
        let film = lib.store.get(2).await.unwrap().probe;
        assert_eq!(
            film.picture_check,
            Some(PictureCheck {
                version: PICTURE_CHECK_VERSION,
                area: None
            }),
            "no bars is a stored result"
        );
        assert!(
            lib.store
                .get(5)
                .await
                .unwrap()
                .probe
                .picture_check
                .is_some(),
            "undetermined is stored too, so a dark film is not decoded every pass"
        );
        assert_eq!(lib.store.get(3).await.unwrap().probe.picture_check, None);
        assert_eq!(lib.store.get(4).await.unwrap().probe.picture_check, None);
    }

    /// A file is decoded once: the second pass over the same library, read
    /// back from the store, decodes nothing; a changed file is redone.
    #[tokio::test]
    async fn a_file_is_decoded_once_until_it_changes() {
        let lib = library(&[(1, "bewitched.mkv", MediaKind::Episode)]).await;
        let fake = Fake {
            script: HashMap::from([(
                "bewitched.mkv".into(),
                Ok(analysis(PictureVerdict::Bars(BARS), 7)),
            )]),
            ..Fake::default()
        };
        run_pass(&lib.store, &fake, &gate(), &lib.items).await;
        assert_eq!(fake.calls().len(), 1);

        let path = lib.items[0].path.clone();
        let (mtime, size) = signature(&path).await.unwrap();
        let scan = lib.store.begin_scan(path.parent().unwrap()).await.unwrap();
        lib.store.mark_seen(1, scan, mtime, size).await.unwrap();

        // A re-probe writes the row again without the check.
        let mut reprobed = lib.store.get(1).await.unwrap();
        reprobed.probe.picture_check = None;
        lib.store.put(reprobed).await.unwrap();

        let items = lib.store.list().await.unwrap();
        assert_eq!(items[0].probe.picture_area(), Some(BARS));
        let sum = run_pass(&lib.store, &fake, &gate(), &items).await;
        assert_eq!((sum.pending, fake.calls().len()), (0, 1));

        std::fs::write(&path, b"a longer file").unwrap();
        let (mtime2, size2) = signature(&path).await.unwrap();
        lib.store.mark_seen(1, scan, mtime2, size2).await.unwrap();
        let items = lib.store.list().await.unwrap();
        assert_eq!(items[0].probe.picture_check, None);
        run_pass(&lib.store, &fake, &gate(), &items).await;
        assert_eq!(fake.calls().len(), 2, "a changed file is redone");
    }

    /// An unreadable file is stored as checked-no-area; a worker crash says
    /// nothing about the file and is retried; a missing file is not offered.
    #[tokio::test]
    async fn an_undecodable_file_is_remembered_and_a_crash_is_retried() {
        let lib = library(&[
            (1, "broken.mkv", MediaKind::Movie),
            (2, "crash.mkv", MediaKind::Movie),
            (3, "gone.mkv", MediaKind::Movie),
        ])
        .await;
        let fake = Fake {
            script: HashMap::from([
                (
                    "broken.mkv".into(),
                    Err("bad input: no video stream".into()),
                ),
                ("crash.mkv".into(), Err("dead".into())),
            ]),
            ..Fake::default()
        };
        let sum = run_pass(&lib.store, &fake, &gate(), &lib.items).await;
        assert_eq!((sum.undecodable, sum.errors, sum.missing), (1, 1, 1));
        assert_eq!(fake.calls(), ["broken.mkv", "crash.mkv"]);
        assert_eq!(
            lib.store.get(1).await.unwrap().probe.picture_check,
            Some(PictureCheck {
                version: PICTURE_CHECK_VERSION,
                area: None
            })
        );
        assert_eq!(lib.store.get(2).await.unwrap().probe.picture_check, None);

        let items = lib.store.list().await.unwrap();
        run_pass(&lib.store, &fake, &gate(), &items).await;
        assert_eq!(
            fake.calls(),
            ["broken.mkv", "crash.mkv", "crash.mkv"],
            "only the crash is retried"
        );
    }

    #[test]
    fn an_older_detector_version_is_redone() {
        let mut item = MediaItem {
            kind: MediaKind::Movie,
            path: "/films/a.mkv".into(),
            ..MediaItem::default()
        };
        assert!(needs_check(&item));
        item.probe.picture_check = Some(PictureCheck {
            version: PICTURE_CHECK_VERSION,
            area: None,
        });
        assert!(!needs_check(&item));
        item.probe.picture_check = Some(PictureCheck {
            version: PICTURE_CHECK_VERSION - 1,
            area: None,
        });
        assert!(needs_check(&item));
        item.path = "ytdlp://youtube/abc".into();
        assert!(!needs_check(&item), "a remote origin is not a file");
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let all = [
            Outcome::Bars,
            Outcome::None,
            Outcome::Undetermined,
            Outcome::Undecodable,
            Outcome::Error,
            Outcome::Missing,
            Outcome::Unscanned,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn the_area_prints_as_a_crop_geometry() {
        assert_eq!(area_string(BARS), "1436x1080+242+0");
    }

    /// The counter, histogram and gauge are the dashboard contract: this
    /// fails if the sweep stops recording any of them, or a label moves.
    #[tokio::test]
    async fn a_pass_records_its_signals() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let lib = library(&[
            (1, "bars.mkv", MediaKind::Episode),
            (2, "none.mkv", MediaKind::Movie),
            (3, "dark.mkv", MediaKind::Movie),
            (4, "broken.mkv", MediaKind::Movie),
            (5, "crash.mkv", MediaKind::Movie),
        ])
        .await;
        let fake = Fake {
            script: HashMap::from([
                (
                    "bars.mkv".into(),
                    Ok(analysis(PictureVerdict::Bars(BARS), 7)),
                ),
                ("none.mkv".into(), Ok(analysis(PictureVerdict::NoBars, 7))),
                (
                    "dark.mkv".into(),
                    Ok(analysis(PictureVerdict::Undetermined, 0)),
                ),
                ("broken.mkv".into(), Err("bad input".into())),
                ("crash.mkv".into(), Err("dead".into())),
            ]),
            ..Fake::default()
        };
        run_pass(&lib.store, &fake, &gate(), &lib.items).await;

        let mut outcomes = Vec::new();
        let (mut pending, mut seconds) = (None, 0usize);
        for (ck, _, _, v) in snapshotter.snapshot().into_vec() {
            match (ck.key().name(), v) {
                ("pharos_picture_check_total", DebugValue::Counter(c)) => {
                    let outcome = ck
                        .key()
                        .labels()
                        .find(|l| l.key() == "outcome")
                        .map(|l| l.value().to_string())
                        .unwrap_or_default();
                    outcomes.push((outcome, c));
                }
                ("pharos_picture_check_pending", DebugValue::Gauge(g)) => {
                    pending = Some(g.into_inner());
                }
                ("pharos_picture_check_seconds", DebugValue::Histogram(h)) => seconds = h.len(),
                _ => {}
            }
        }
        outcomes.sort();
        assert_eq!(
            outcomes,
            [
                ("bars".to_owned(), 1),
                ("error".to_owned(), 1),
                ("none".to_owned(), 1),
                ("undecodable".to_owned(), 1),
                ("undetermined".to_owned(), 1),
            ]
        );
        assert_eq!(pending, Some(5.0));
        assert_eq!(seconds, 5, "one timing per checked file");
    }
}
