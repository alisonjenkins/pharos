//! LIB-A9 — tiered library change-detection + graceful fallback.
//!
//! Each configured media root independently picks the best change-detection
//! mode it can sustain, logged once at startup:
//!
//! - **Native watch** (highest tier): a filesystem watcher (inotify / kqueue /
//!   ReadDirectoryChangesW via [`pharos_scanner::spawn_watch`], behind the
//!   `watch` feature). Chosen only when the `watch` feature is built, the
//!   operator left `library_watch_enabled = true`, and the root's filesystem
//!   can actually deliver events. Network mounts (NFS / SMB / CIFS) and FUSE
//!   filesystems can't, so they are detected up front via `statfs` and skip
//!   straight to the poll tier. A still-eligible root whose watcher fails to
//!   *initialise* (e.g. inotify limit) also falls back, and a live watcher
//!   that later errors / closes downgrades to the poll tier at runtime (never
//!   crashes — V6 spirit).
//!
//! - **Periodic rescan** (fallback tier): a timer task that runs one immediate
//!   scan on boot (so a fresh deploy is populated at once, not a poll interval
//!   later) and then re-runs the cheap incremental
//!   [`pharos_scanner::FsScanner::scan_into`] every `library_poll_interval_secs`.
//!   This backstops *every* root (it also runs alongside a native watch as a
//!   safety net for missed events) and is the primary detector for network/fuse
//!   roots or when the `watch` feature is off. `library_poll_interval_secs = 0`
//!   disables it (and with it the boot scan — use a `scan` CronJob instead).
//!
//! - **Manual refresh** (floor tier): if both the watch and the poll tiers are
//!   disabled for a root, it only updates on an admin `POST /Library/Refresh`.
//!
//! Every tier broadcasts the same added/removed deltas to connected `/socket`
//! clients via [`crate::state::AppState::notify_library_delta`] (A4), so a
//! file that lands via a watch event reaches client UIs identically to one
//! picked up by a manual refresh.
//!
//! The module compiles + no-ops gracefully when the `watch` feature is off:
//! the watch tier is simply never selected and only the periodic + manual
//! tiers remain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use actix_web::web;
use pharos_core::Prober;
use pharos_scanner::{FsScanner, RootWatchability};

use crate::state::AppState;

/// The change-detection tier chosen for one media root. Logged at startup and
/// (for `watch`) capable of downgrading to `Periodic` at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootMode {
    /// Native filesystem watch active; periodic rescan also runs as a
    /// safety-net backstop at `interval`.
    Watch,
    /// Periodic incremental rescan only (network/fuse root, watch disabled,
    /// or watch feature not built).
    Periodic,
    /// Neither watch nor periodic — manual `/Library/Refresh` only.
    Manual,
}

/// LIB-A9 — knobs resolved from config, handed to [`spawn_for_roots`].
#[derive(Debug, Clone, Copy)]
pub struct WatchConfig {
    /// Operator's `library_watch_enabled` toggle. When false, no root takes
    /// the watch tier regardless of filesystem support.
    pub watch_enabled: bool,
    /// Periodic-rescan interval. `Duration::ZERO` disables the poll tier.
    pub poll_interval: Duration,
    /// Per-probe rate-limit (ms) threaded into every spawned scanner so a
    /// background rescan honours the same throttle as `/Library/Refresh`.
    pub rate_limit_ms: u64,
}

/// LIB-A9 — decide a root's tier from its filesystem watchability + config,
/// *without* attempting a watch. Pure (modulo the caller's `statfs` result) so
/// the fallback selection is unit-testable: feed a `RootWatchability` +
/// `WatchConfig` and assert the tier. The actual watch-init may still downgrade
/// `Watch` → `Periodic` (init failure) at spawn time.
pub fn plan_mode(
    watchability: RootWatchability,
    cfg: &WatchConfig,
    watch_feature: bool,
) -> RootMode {
    let poll_on = !cfg.poll_interval.is_zero();
    let watch_on = watch_feature && cfg.watch_enabled && watchability.watch_eligible();
    match (watch_on, poll_on) {
        (true, _) => RootMode::Watch,
        (false, true) => RootMode::Periodic,
        (false, false) => RootMode::Manual,
    }
}

/// LIB-A9 — bring up change-detection for every media root. `scanner_for`
/// produces a fresh, owned [`FsScanner`] per root (each spawned task needs its
/// own — `FsScanner` carries the prober, which isn't required to be `Clone`).
///
/// Returns a `Vec` of per-root [`RootMode`]s in the same order as `roots`,
/// mostly for tests + observability; the spawned tasks live for the process
/// lifetime (or until the returned `WatchGuards` are dropped).
pub fn spawn_for_roots<P, F>(
    state: web::Data<AppState>,
    roots: &[PathBuf],
    cfg: WatchConfig,
    mut scanner_for: F,
) -> WatchGuards
where
    P: Prober + Send + Sync + 'static,
    F: FnMut() -> FsScanner<P>,
{
    let mut guards = WatchGuards::default();
    for root in roots {
        let watchability = pharos_scanner::detect_root_watchability(root);
        let mode = plan_mode(watchability, &cfg, cfg!(feature = "watch"));
        tracing::info!(
            root = %root.display(),
            ?watchability,
            ?mode,
            poll_secs = cfg.poll_interval.as_secs(),
            "library change-detection mode selected",
        );
        spawn_one(
            &mut guards,
            &state,
            root.clone(),
            mode,
            cfg,
            &mut scanner_for,
        );
    }
    guards
}

/// Spawn the task(s) for a single root per its planned `mode`. Watch roots get
/// a watcher *and* a backstop periodic task; periodic roots get just the timer;
/// manual roots get nothing. `scanner_for` mints a fresh owned scanner each
/// call (the prober isn't required to be `Clone`, so the watch tier — which
/// needs two scanners — calls it twice).
fn spawn_one<P, F>(
    guards: &mut WatchGuards,
    state: &web::Data<AppState>,
    root: PathBuf,
    mode: RootMode,
    cfg: WatchConfig,
    scanner_for: &mut F,
) where
    P: Prober + Send + Sync + 'static,
    F: FnMut() -> FsScanner<P>,
{
    match mode {
        RootMode::Watch => {
            // The watch tier still runs the periodic rescan as a safety net
            // (missed events, channel overflow). Watcher + backstop each own a
            // freshly-minted scanner.
            #[cfg(feature = "watch")]
            {
                match spawn_watch_task(state.clone(), root.clone(), scanner_for()) {
                    Ok(handle) => {
                        guards.watchers.push(handle);
                    }
                    Err(()) => {
                        // Init failure — downgrade to periodic (or manual).
                        tracing::warn!(
                            root = %root.display(),
                            "native watch init failed; falling back to periodic rescan",
                        );
                    }
                }
                // The backstop periodic rescan runs alongside a live watch and
                // is also the sole detector after a watch-init failure.
                if !cfg.poll_interval.is_zero() {
                    spawn_periodic(state.clone(), root, cfg.poll_interval, scanner_for());
                }
            }
            // watch feature not built: plan_mode never returns Watch, so this
            // is unreachable, but fall through to periodic defensively.
            #[cfg(not(feature = "watch"))]
            {
                let _ = guards;
                if !cfg.poll_interval.is_zero() {
                    spawn_periodic(state.clone(), root, cfg.poll_interval, scanner_for());
                }
            }
        }
        RootMode::Periodic => {
            spawn_periodic(state.clone(), root, cfg.poll_interval, scanner_for());
        }
        RootMode::Manual => {
            // Nothing to spawn — admin /Library/Refresh is the only trigger.
        }
    }
}

/// Spawn the boot + periodic incremental-rescan task for one root. Runs one
/// immediate scan on startup (so the library is populated without waiting a
/// poll interval), then rescans every `interval`. Each scan runs `scan_into`
/// and broadcasts any delta. Errors are logged and the loop continues (V6) —
/// a transient scan failure never kills the schedule.
fn spawn_periodic<P>(
    state: web::Data<AppState>,
    root: PathBuf,
    interval: Duration,
    scanner: FsScanner<P>,
) where
    P: Prober + Send + Sync + 'static,
{
    actix_web::rt::spawn(async move {
        // Boot scan: populate the library immediately rather than leaving a
        // fresh deploy empty until the first poll interval elapses. This is
        // what previously forced a chart `scan` initContainer to gate server
        // readiness on a full media scan — the server owns it now, in-process
        // and non-blocking (it runs on the tokio pool while `serve` finishes
        // binding; SQLite stays single-writer since this is that writer).
        run_scan_and_broadcast(&state, &root, &scanner, "startup").await;
        let mut ticker = tokio::time::interval(interval);
        // Consume the immediate first tick — the boot scan above already
        // covered t=0, so the next *periodic* rescan is due one full interval
        // later.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            run_scan_and_broadcast(&state, &root, &scanner, "periodic").await;
        }
    });
}

/// Run one incremental `scan_into` over `root` and broadcast its delta.
async fn run_scan_and_broadcast<P>(state: &AppState, root: &Path, scanner: &FsScanner<P>, why: &str)
where
    P: Prober + Send + Sync + 'static,
{
    match scanner.scan_into(root, &state.stores).await {
        Ok(outcome) => {
            let touched = outcome.added.len() + outcome.updated.len() + outcome.removed.len();
            if touched > 0 {
                tracing::info!(
                    root = %root.display(),
                    why,
                    added = outcome.added.len(),
                    updated = outcome.updated.len(),
                    removed = outcome.removed.len(),
                    skipped = outcome.skipped,
                    "library rescan applied a delta",
                );
                // LIB-C1 — re-stamp media_items.library_id by path-prefix so
                // freshly-added items land in the right typed library (the
                // /Items?ParentId=<library id> pivot resolves via the
                // indexed join, not just the boot-time backfill). Idempotent;
                // a backend error here is non-fatal to the rescan.
                if !outcome.added.is_empty() {
                    use pharos_core::LibraryStore;
                    if let Err(e) = state.stores.backfill_library_ids().await {
                        tracing::warn!(error = %e, "library_id backfill after rescan failed");
                    }
                }
                broadcast_outcome(state, &outcome);
            }
            // Extract text subtitles for new/changed items NOW, at scan time,
            // so the first playback never eats a ~30 s cold whole-file demux
            // over NFS (a subtitle stream is sparse across the whole container,
            // so extraction reads the entire multi-GB file). Bounded
            // concurrency keeps this under the NFS I/O ceiling — a lesson from
            // the scan crash-loop where unbounded whole-file reads starved live
            // playback. The persistent subtitle cache is mtime-keyed, so each
            // file version is extracted at most once ever.
            warm_scanned_subtitles(state, &outcome).await;
        }
        Err(e) => {
            tracing::warn!(root = %root.display(), why, error = %e, "library rescan failed");
        }
    }
}

/// Pre-extract the text subtitles of the freshly added/updated items into the
/// (persistent) subtitle cache, so a viewer's first Stream.vtt / Stream.js /
/// Stream.ass fetch is a warm-cache hit instead of a cold ~30 s demux.
async fn warm_scanned_subtitles(state: &AppState, outcome: &pharos_core::ScanOutcome) {
    let ids: Vec<u64> = outcome
        .added
        .iter()
        .chain(outcome.updated.iter())
        .copied()
        .collect();
    // Scan-context: these files were just read anyway, so don't park on the
    // playback gate — warm them promptly.
    warm_item_subtitles(state, ids, true, "scan-time").await;
}

/// Delay before the startup library-wide subtitle warm begins, so it doesn't
/// pile onto boot I/O.
const WARM_ALL_DELAY: Duration = Duration::from_secs(60);

/// Spawn a one-shot, playback-gated pass that warms EVERY already-indexed
/// item's text subtitles into the persistent cache. Scan-time warming only
/// covers newly added/changed items; this backfills the existing library so a
/// viewer's first play of any title finds warm subs (once — the disk cache
/// then keeps them warm across restarts). Bounded + gated so the whole-file
/// demuxes stay off the live-playback I/O path.
pub fn spawn_subtitle_warm_all(state: web::Data<AppState>) {
    if state.subtitles.is_none() {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(WARM_ALL_DELAY).await;
        use pharos_core::MediaStore;
        let ids: Vec<u64> = match state.stores.list().await {
            Ok(items) => items
                .iter()
                .filter(|i| !i.probe.subtitle_tracks.is_empty())
                .map(|i| i.id)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "subtitle warm-all: item list failed");
                return;
            }
        };
        warm_item_subtitles(&state, ids, true, "library warm-all").await;
    });
}

/// Warm ONE item's text subtitles immediately and UNGATED — called from
/// PlaybackInfo. The item is being watched right now, so its subs must warm
/// before the client toggles them; unlike the bulk warm-all this does not park
/// on the playback gate (it's a single item, and it's the very file already
/// being segmented, so its pages are hot). Idempotent via the persistent cache.
pub fn spawn_warm_item_subtitles(state: web::Data<AppState>, id: u64) {
    if state.subtitles.is_none() {
        return;
    }
    tokio::spawn(async move {
        warm_item_subtitles(&state, vec![id], false, "playback priority").await;
    });
}

/// Warm the given items' text subtitles into the cache. Each whole-file demux
/// is a heavy NFS read, so when `throttle` is set every task takes a permit
/// from the shared adaptive [`AppState::bg_io`] gate — which the regulator
/// squeezes down to a trickle whenever a client is streaming, so a bulk warm
/// (scan-time / library-wide) keeps making progress but never starves a live
/// stream. `throttle = false` bypasses the gate for the single item being
/// actively played (its subs must warm promptly before the viewer toggles
/// them — see [`spawn_warm_item_subtitles`]).
async fn warm_item_subtitles(state: &AppState, ids: Vec<u64>, throttle: bool, why: &str) {
    use pharos_core::MediaStore;
    let Some(cache) = state.subtitles.clone() else {
        return;
    };
    if ids.is_empty() {
        return;
    }
    let started = std::time::Instant::now();
    let mut tasks = Vec::with_capacity(ids.len());
    for id in ids {
        let cache = cache.clone();
        let stores = state.stores.clone();
        // The adaptive gate replaces both the old fixed warm-concurrency cap
        // and the all-or-nothing playback quiet-gate: it bounds concurrency
        // AND yields to playback, in one primitive.
        let bg = throttle.then(|| state.bg_io.clone());
        tasks.push(tokio::spawn(async move {
            let _permit = match &bg {
                Some(sem) => sem.clone().acquire_owned().await.ok(),
                None => None,
            };
            match stores.get(id).await {
                Ok(item) if !item.probe.subtitle_tracks.is_empty() => {
                    crate::api::jellyfin::subtitles::pre_extract_subtitles(&cache, &item).await;
                }
                _ => {}
            }
        }));
    }
    let n = tasks.len();
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!(
        items = n,
        why,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "subtitle warm pass complete"
    );
}

/// Relay a [`pharos_core::ScanOutcome`] to connected `/socket` clients. An
/// in-place update also invalidates client caches, so it is relayed as an
/// "added" id (matching the `/Library/Refresh` handler's convention) to make
/// jellyfin-web re-fetch the changed item.
fn broadcast_outcome(state: &AppState, outcome: &pharos_core::ScanOutcome) {
    let mut added = outcome.added.clone();
    added.extend(outcome.updated.iter().copied());
    state.notify_library_delta(&added, &outcome.removed);
}

/// Opaque keep-alive guards for the spawned watch tasks. Dropping it stops the
/// native watchers (the periodic tasks are detached and run for the process
/// lifetime). Held by `serve` so the watches outlive `serve`'s setup scope.
#[derive(Default)]
pub struct WatchGuards {
    #[cfg(feature = "watch")]
    watchers: Vec<pharos_scanner::WatchHandle>,
}

#[cfg(feature = "watch")]
impl WatchGuards {
    /// Number of live native watchers — for tests / observability.
    pub fn watcher_count(&self) -> usize {
        self.watchers.len()
    }
}

/// LIB-A9 — start a native filesystem watch for `root`, store its handle in
/// `guards`, and spawn a consumer task that broadcasts each settled batch's
/// delta. Returns `Err(())` if the watcher fails to initialise (caller falls
/// back to periodic). A live watcher whose update channel later closes
/// (backend died) just ends its consumer task; the backstop periodic rescan
/// keeps the root current (graceful downgrade, no crash — V6).
#[cfg(feature = "watch")]
fn spawn_watch_task<P>(
    state: web::Data<AppState>,
    root: PathBuf,
    scanner: FsScanner<P>,
) -> Result<pharos_scanner::WatchHandle, ()>
where
    P: Prober + Send + Sync + 'static,
{
    use pharos_scanner::{spawn_watch, WatchOptions};

    let mut handle = match spawn_watch(
        root.clone(),
        state.stores.clone(),
        scanner,
        WatchOptions::default(),
    ) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "spawn_watch failed");
            return Err(());
        }
    };

    // Split the update receiver out so a consumer task can broadcast each
    // settled batch's delta while the returned handle keeps the OS watch +
    // debounce task alive (the handle's Drop aborts both). `WatchHandle.updates`
    // is a public field; swap in a fresh already-closed receiver (its sender is
    // dropped immediately) so the handle stays valid but inert.
    let (dead_tx, dead_rx) = tokio::sync::mpsc::channel(1);
    drop(dead_tx);
    let mut updates = std::mem::replace(&mut handle.updates, dead_rx);

    let state_for_consumer = state.clone();
    let root_for_consumer = root.clone();
    actix_web::rt::spawn(async move {
        while let Some(outcome) = updates.recv().await {
            let touched = outcome.added.len() + outcome.updated.len() + outcome.removed.len();
            if touched > 0 {
                tracing::debug!(
                    root = %root_for_consumer.display(),
                    added = outcome.added.len(),
                    updated = outcome.updated.len(),
                    removed = outcome.removed.len(),
                    "watch batch delta",
                );
                broadcast_outcome(&state_for_consumer, &outcome);
            }
        }
        tracing::info!(
            root = %root_for_consumer.display(),
            "watch update channel closed; periodic rescan remains as backstop",
        );
    });

    Ok(handle)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn cfg(watch_enabled: bool, poll_secs: u64) -> WatchConfig {
        WatchConfig {
            watch_enabled,
            poll_interval: Duration::from_secs(poll_secs),
            rate_limit_ms: 0,
        }
    }

    /// Scan-time subtitle warm must populate the cache so the first playback
    /// fetch is warm, not a ~30 s cold demux. ffmpeg-gated: muxes a real subrip
    /// MKV + extracts it.
    #[tokio::test]
    #[ignore = "spawns ffmpeg to mux + extract a subtitle"]
    async fn scan_time_warm_populates_subtitle_cache() {
        use crate::state::Stores;
        use pharos_cache::subtitle_cache::{mtime_secs, SubtitleKind};
        use pharos_cache::SubtitleCache;
        use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore, SubtitleTrack};

        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        // A subrip sidecar muxed as embedded stream 2 (video 0, audio 1, sub 2).
        let srt = dir.path().join("s.srt");
        std::fs::write(&srt, "1\n00:00:00,500 --> 00:00:02,000\nwarm me\n").unwrap();
        let mkv = dir.path().join("clip.mkv");
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i", "testsrc=d=3:s=64x64:r=5"])
            .args(["-f", "lavfi", "-i", "sine=d=3"])
            .arg("-i")
            .arg(&srt)
            .args(["-map", "0:v", "-map", "1:a", "-map", "2"])
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-c:s",
                "copy",
            ])
            .arg(&mkv)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg mux failed");

        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        stores
            .put(MediaItem {
                id: 5,
                path: mkv.clone(),
                title: "c".into(),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    subtitle_tracks: vec![SubtitleTrack {
                        stream_index: 2,
                        codec: Some("subrip".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
        let cache =
            SubtitleCache::new(64 * 1024 * 1024, 1024).with_disk(dir.path().join("subcache"));
        let state = AppState::new(stores, "t".into()).with_subtitle_cache(cache);

        // Cold before the warm.
        let mtime = mtime_secs(&mkv).await;
        assert!(
            state
                .subtitles
                .as_ref()
                .unwrap()
                .get(&mkv, mtime, 2, SubtitleKind::Embedded)
                .await
                .is_none(),
            "cache must start cold"
        );

        let outcome = pharos_core::ScanOutcome {
            added: vec![5],
            updated: vec![],
            removed: vec![],
            skipped: 0,
        };
        warm_scanned_subtitles(&state, &outcome).await;

        // Warm after: the first real Stream.vtt/js fetch is now a cache hit.
        assert!(
            state
                .subtitles
                .as_ref()
                .unwrap()
                .get(&mkv, mtime, 2, SubtitleKind::Embedded)
                .await
                .is_some(),
            "scan-time warm must populate the subtitle cache"
        );
    }

    #[test]
    fn network_root_plans_periodic_even_with_watch_on() {
        // LIB-A9 — a network/fuse root is never watch-eligible, so even with
        // the watch feature + flag on it must plan the periodic tier.
        let m = plan_mode(RootWatchability::Network, &cfg(true, 300), true);
        assert_eq!(m, RootMode::Periodic);
        let m = plan_mode(RootWatchability::Fuse, &cfg(true, 300), true);
        assert_eq!(m, RootMode::Periodic);
    }

    #[test]
    fn local_root_plans_watch_when_feature_and_flag_on() {
        let m = plan_mode(RootWatchability::Watchable, &cfg(true, 300), true);
        assert_eq!(m, RootMode::Watch);
        // Unknown (non-Linux / statfs-failed) is also eligible.
        let m = plan_mode(RootWatchability::Unknown, &cfg(true, 300), true);
        assert_eq!(m, RootMode::Watch);
    }

    #[test]
    fn watch_disabled_falls_to_periodic() {
        // Flag off → periodic regardless of filesystem.
        let m = plan_mode(RootWatchability::Watchable, &cfg(false, 300), true);
        assert_eq!(m, RootMode::Periodic);
    }

    #[test]
    fn feature_off_never_plans_watch() {
        // With the watch feature not built, a watchable local root still plans
        // periodic (the graceful no-op the brief requires).
        let m = plan_mode(RootWatchability::Watchable, &cfg(true, 300), false);
        assert_eq!(m, RootMode::Periodic);
    }

    #[test]
    fn poll_zero_and_no_watch_is_manual() {
        // Both tiers disabled → the manual floor.
        let m = plan_mode(RootWatchability::Watchable, &cfg(true, 0), false);
        assert_eq!(m, RootMode::Manual);
        let m = plan_mode(RootWatchability::Network, &cfg(true, 0), true);
        assert_eq!(m, RootMode::Manual);
    }
}
