//! LIB-A8 — filesystem watcher. Keeps a media root's index live between
//! full scans by reacting to OS filesystem events (inotify / kqueue /
//! ReadDirectoryChangesW via the `notify` crate).
//!
//! Design:
//! - `notify` runs its own OS thread and hands events to a callback. We
//!   marshal those onto a tokio mpsc channel so the async side never touches
//!   the notify thread's stack, and no blocking FS call sits on the reactor
//!   (V5). The per-file stat/probe/fingerprint work happens on the tokio
//!   task (probe/fingerprint already marshal their blocking IO onto
//!   `spawn_blocking`).
//! - Events are **debounced**: a burst (an editor writing a file, a copy
//!   landing in chunks, an `mv` firing create+remove) is coalesced over a
//!   short quiet window before we act, so we probe a settled file once.
//! - **Create / modify** of a single file routes through
//!   [`FsScanner::update_path`] — the same stat → skip-check → move-detect →
//!   probe → put → mark_seen logic the full scan uses, on one path.
//! - **Remove / rename** can't be reconciled from a single path safely (a
//!   rename is a remove+create pair; a delete needs the root-scoped sweep to
//!   stay atomic, V10). When a debounced batch contains any remove/rename we
//!   fall back to a single incremental [`FsScanner::scan_into`] over the
//!   root, which already performs the atomic root-scoped sweep + move
//!   detection and yields the same [`ScanOutcome`] delta.
//! - Each settled batch produces a [`ScanOutcome`] delivered on the handle's
//!   `updates` channel, so the server (A9) can broadcast the same
//!   added/removed deltas as a manual refresh.
//! - A watcher error is surfaced (not panicked) so A9 can downgrade the root
//!   to polling (V6 spirit: degrade, never crash).

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use pharos_core::{MediaStore, Prober, ScanOutcome};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::fs::{FsScanner, PathUpdate};

/// Default quiet window: events stop arriving for this long before a batch
/// is processed. Keeps a chunked copy / editor save-dance from triggering
/// several probes of a half-written file.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(750);

/// LIB-A8 — knobs for [`spawn_watch`]. `Default` matches the production
/// shape: recursive watch, the default debounce window.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Quiet-window length before a coalesced batch is acted on.
    pub debounce: Duration,
    /// Capacity of the internal event channel from the notify thread to the
    /// async task. A full channel drops events (logged) rather than blocking
    /// the notify thread; a later full scan reconciles anything missed.
    pub channel_capacity: usize,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            channel_capacity: 1024,
        }
    }
}

/// LIB-A8 — why a watch couldn't be established (or died). A9 keys its
/// poll-fallback decision off [`WatchError::Unsupported`]: the backing
/// filesystem can't deliver events here (a network mount, a FUSE fs, an
/// inotify-limit exhaustion), so the root must be polled instead. Every
/// other variant is a genuine error worth surfacing.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The platform / filesystem cannot watch this path — the caller should
    /// fall back to periodic polling. Carries the underlying message.
    #[error("filesystem watch unsupported for this root: {0}")]
    Unsupported(String),
    /// The watcher backend failed to initialise or register the path for a
    /// reason other than lack of support.
    #[error("failed to start filesystem watch: {0}")]
    Backend(String),
}

impl WatchError {
    /// Classify a `notify::Error` into "unsupported here" (→ poll fallback)
    /// vs a generic backend error. inotify/kqueue surface support gaps as
    /// `MaxFilesWatch` (limit hit) or an `io` error with a recognisable kind
    /// (e.g. `Unsupported`, `NotFound` on a vanished mount).
    fn from_notify(err: notify::Error) -> Self {
        use notify::ErrorKind;
        match &err.kind {
            ErrorKind::MaxFilesWatch => WatchError::Unsupported(err.to_string()),
            ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::Unsupported => {
                WatchError::Unsupported(err.to_string())
            }
            _ => WatchError::Backend(err.to_string()),
        }
    }
}

/// LIB-A8 — handle to a running watch task. Dropping it stops the watch:
/// the `notify` watcher is dropped (unregistering the OS watch) and the
/// async task observes the closed event channel and exits. Holding the
/// handle keeps the watch alive.
pub struct WatchHandle {
    /// Receiver for the [`ScanOutcome`] of each settled batch. The server
    /// (A9) consumes these and broadcasts the deltas. Optional so the owner
    /// can `take()` it and move it into its own consumer task.
    pub updates: mpsc::Receiver<ScanOutcome>,
    /// Kept alive to hold the OS watch open; dropped with the handle.
    _watcher: RecommendedWatcher,
    /// The debounce/process task. Aborted on drop so we don't leak it.
    task: JoinHandle<()>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// LIB-A8 — start watching `root`. On create/modify the affected files are
/// incrementally updated in `store`; on remove/rename a root-scoped
/// incremental scan reconciles the deletion. Each settled batch's
/// [`ScanOutcome`] is delivered on the returned handle's `updates` channel.
///
/// `scanner` carries the prober + extension filter + rate limit; it is moved
/// into the watch task. Returns [`WatchError::Unsupported`] when the backing
/// filesystem can't deliver events, so the caller (A9) can fall back to
/// polling. Never panics — a per-event probe/IO failure is logged and the
/// loop continues (V6).
pub fn spawn_watch<P, S>(
    root: PathBuf,
    store: S,
    scanner: FsScanner<P>,
    opts: WatchOptions,
) -> Result<WatchHandle, WatchError>
where
    P: Prober + Send + Sync + 'static,
    S: MediaStore + pharos_core::GenreStore + Send + Sync + 'static,
{
    // Bridge the notify OS thread → async task. notify's callback runs on
    // its own thread; `try_send` never blocks it (a full channel drops the
    // event, logged — a later full scan reconciles).
    let (raw_tx, raw_rx) = mpsc::channel::<Event>(opts.channel_capacity);
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| match res {
            Ok(ev) => {
                if raw_tx.try_send(ev).is_err() {
                    tracing::warn!("watch event channel full or closed; dropping event");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "watch backend reported an error event");
            }
        },
        NotifyConfig::default(),
    )
    .map_err(WatchError::from_notify)?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(WatchError::from_notify)?;

    let (out_tx, out_rx) = mpsc::channel::<ScanOutcome>(64);
    let task = tokio::spawn(watch_loop(root, store, scanner, opts, raw_rx, out_tx));

    Ok(WatchHandle {
        updates: out_rx,
        _watcher: watcher,
        task,
    })
}

/// The debounce + process loop. Drains the raw event channel, coalesces a
/// burst over the quiet window, then acts on the settled batch.
async fn watch_loop<P, S>(
    root: PathBuf,
    store: S,
    scanner: FsScanner<P>,
    opts: WatchOptions,
    mut raw_rx: mpsc::Receiver<Event>,
    out_tx: mpsc::Sender<ScanOutcome>,
) where
    P: Prober + Send + Sync + 'static,
    S: MediaStore + pharos_core::GenreStore + Send + Sync + 'static,
{
    let exts = scanner.extensions_snapshot();
    loop {
        // Block until the first event of a burst arrives (or the channel
        // closes — handle dropped — in which case we exit cleanly).
        let Some(first) = raw_rx.recv().await else {
            tracing::debug!(root = %root.display(), "watch channel closed; stopping watcher");
            return;
        };

        let mut batch = Batch::default();
        batch.absorb(&first);

        // Debounce: keep absorbing until the channel stays quiet for the
        // whole window. Each new event resets the timer (coalesce a burst).
        loop {
            match tokio::time::timeout(opts.debounce, raw_rx.recv()).await {
                Ok(Some(ev)) => batch.absorb(&ev),
                // Quiet window elapsed with no new event — batch is settled.
                Err(_) => break,
                // Channel closed mid-burst: process what we have, then exit
                // after this batch.
                Ok(None) => break,
            }
        }

        match process_batch(&root, &store, &scanner, &exts, batch).await {
            Ok(Some(outcome)) => {
                // Best-effort delivery: a closed/full updates channel just
                // means nobody's listening — the store was still updated.
                let _ = out_tx.try_send(outcome);
            }
            Ok(None) => {}
            Err(err) => {
                // A store-level failure. Log + keep watching; never panic (V6).
                tracing::error!(
                    root = %root.display(),
                    error = %err,
                    "watch batch failed to apply; continuing",
                );
            }
        }
    }
}

/// A coalesced burst of filesystem events, reduced to the paths we care
/// about + whether any removal/rename happened (which forces the full-scan
/// reconcile path).
#[derive(Default)]
struct Batch {
    /// Created/modified paths to (re)index incrementally.
    touched: Vec<PathBuf>,
    /// Any remove/rename in the batch → fall back to a root-scoped scan so
    /// the atomic sweep + move-detection runs (V10).
    needs_reconcile: bool,
}

impl Batch {
    fn absorb(&mut self, ev: &Event) {
        match ev.kind {
            EventKind::Create(_) | EventKind::Modify(notify::event::ModifyKind::Data(_)) => {
                self.touched.extend(ev.paths.iter().cloned());
            }
            // A rename is delivered as a Modify(Name) (and/or Remove); both
            // mean a path moved → reconcile via full scan (handles the move
            // by fingerprint + sweeps the old path).
            EventKind::Modify(notify::event::ModifyKind::Name(_)) | EventKind::Remove(_) => {
                self.needs_reconcile = true;
                // Still record the paths: a rename's *new* name may be a
                // create we want to index even on the reconcile path.
                self.touched.extend(ev.paths.iter().cloned());
            }
            // Metadata-only / Any / Other: ignore — no content change.
            _ => {}
        }
    }
}

/// Act on one settled batch. Returns the batch's delta [`ScanOutcome`], or
/// `None` when nothing relevant changed.
async fn process_batch<P, S>(
    root: &Path,
    store: &S,
    scanner: &FsScanner<P>,
    exts: &std::collections::HashSet<String>,
    batch: Batch,
) -> pharos_core::DomainResult<Option<ScanOutcome>>
where
    P: Prober + Send + Sync + 'static,
    S: MediaStore + pharos_core::GenreStore + Send + Sync + 'static,
{
    // Remove/rename in the batch → defer to a full incremental scan of the
    // root. It performs the atomic root-scoped sweep + move detection and
    // returns the proper added/updated/removed delta.
    if batch.needs_reconcile {
        let outcome = scanner.scan_into(root, store).await?;
        return Ok(Some(outcome));
    }

    // Pure create/modify batch: incrementally update each touched media file
    // by path. Dedup + filter to recognised extensions (the watch is
    // recursive over the whole tree, so directory + non-media events arrive).
    let mut paths: Vec<PathBuf> = batch.touched;
    paths.sort_unstable();
    paths.dedup();

    let scan_id = store.begin_scan(root).await?;
    let mut outcome = ScanOutcome::default();
    let mut touched_media = false;
    for path in paths {
        if !is_media_file(&path, exts) {
            continue;
        }
        // Skip directories / vanished entries: a create event for a new
        // subdir carries the dir path. `update_path` would stat-fail and
        // skip anyway, but filtering here avoids a needless probe attempt.
        if !tokio::fs::metadata(&path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        touched_media = true;
        match scanner.update_path(&path, store, scan_id).await {
            Ok(PathUpdate::Added(id)) => outcome.added.push(id),
            Ok(PathUpdate::Updated(id)) => outcome.updated.push(id),
            Ok(PathUpdate::Skipped) => outcome.skipped += 1,
            Err(err) => {
                // Store failure on one path — log + continue (V6). Other
                // paths in the batch still get a chance.
                tracing::warn!(error = %err, "watch: update_path failed for one file");
            }
        }
    }
    // Close the scan token for observability parity with scan_into. No sweep
    // on a create/modify-only batch.
    let seen = (outcome.added.len() + outcome.updated.len() + outcome.skipped) as i64;
    store.finish_scan(scan_id, seen, 0).await?;

    if !touched_media {
        return Ok(None);
    }
    Ok(Some(outcome))
}

/// True when `path`'s lowercased extension is one we index.
fn is_media_file(path: &Path, exts: &std::collections::HashSet<String>) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .map(|e| exts.contains(&e))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{
        DomainError, DomainResult, Fingerprint, MediaId, MediaItem, MediaKind, ProbeInfo, ScanState,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// Clone-able in-memory `MediaStore` for watcher tests. Mirrors the
    /// scanner's `fs::tests::MemStore` semantics but shares state across
    /// clones (an `Arc`) so the test can keep one handle to assert against
    /// while the watch task owns another.
    #[derive(Clone, Default)]
    struct MemStore(Arc<MemInner>);

    #[derive(Default)]
    struct MemInner {
        items: Mutex<HashMap<MediaId, MediaItem>>,
        states: Mutex<HashMap<MediaId, ScanState>>,
        fps: Mutex<HashMap<MediaId, Fingerprint>>,
        next_scan_id: AtomicI64,
        // LIB-C4 — item_genres mirror (unused by assertions, satisfies the bound).
        item_genres: Mutex<HashMap<MediaId, Vec<String>>>,
    }

    impl MediaStore for MemStore {
        async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
            self.0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .cloned()
                .ok_or(DomainError::NotFound(id))
        }
        async fn put(&self, item: MediaItem) -> DomainResult<()> {
            self.0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(item.id, item);
            Ok(())
        }
        async fn list(&self) -> DomainResult<Vec<MediaItem>> {
            Ok(self
                .0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .values()
                .cloned()
                .collect())
        }
        async fn scan_state(&self, id: MediaId) -> DomainResult<Option<ScanState>> {
            Ok(self
                .0
                .states
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .copied())
        }
        async fn begin_scan(&self, _root: &Path) -> DomainResult<i64> {
            Ok(self.0.next_scan_id.fetch_add(1, Ordering::SeqCst) + 1)
        }
        async fn mark_seen(
            &self,
            id: MediaId,
            scan_id: i64,
            mtime: i64,
            size: u64,
        ) -> DomainResult<()> {
            if !self
                .0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .contains_key(&id)
            {
                return Ok(());
            }
            self.0
                .states
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(
                    id,
                    ScanState {
                        last_scanned: scan_id,
                        file_mtime: mtime,
                        file_size: size,
                        last_seen_scan_id: scan_id,
                    },
                );
            Ok(())
        }
        async fn sweep_unseen(
            &self,
            scan_id: i64,
            root_prefix: &str,
        ) -> DomainResult<Vec<MediaId>> {
            let mut items = self
                .0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let states = self
                .0
                .states
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let base = root_prefix.strip_suffix('/').unwrap_or(root_prefix);
            let under_root = format!("{base}/");
            let doomed: Vec<MediaId> = items
                .iter()
                .filter(|(id, item)| {
                    item.path.to_string_lossy().starts_with(&under_root)
                        && states.get(*id).map(|s| s.last_seen_scan_id) != Some(scan_id)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in &doomed {
                items.remove(id);
            }
            Ok(doomed)
        }
        async fn finish_scan(&self, _: i64, _: i64, _: i64) -> DomainResult<()> {
            Ok(())
        }
        async fn find_by_fp(&self, fp: Fingerprint) -> DomainResult<Option<MediaItem>> {
            let fps = self
                .0
                .fps
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let items = self
                .0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut matches: Vec<MediaId> = fps
                .iter()
                .filter(|(_, v)| **v == fp)
                .map(|(id, _)| *id)
                .collect();
            matches.sort_unstable();
            Ok(matches.into_iter().find_map(|id| items.get(&id).cloned()))
        }
        async fn set_fingerprint(&self, id: MediaId, fp: Fingerprint) -> DomainResult<()> {
            if !self
                .0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .contains_key(&id)
            {
                return Ok(());
            }
            self.0
                .fps
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(id, fp);
            Ok(())
        }
        async fn rebind_path(&self, id: MediaId, new_path: &Path) -> DomainResult<()> {
            if let Some(item) = self
                .0
                .items
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get_mut(&id)
            {
                item.path = new_path.to_path_buf();
            }
            Ok(())
        }
    }

    // LIB-C4 — watcher tests don't assert on the genre join, so this is a
    // lightweight in-memory mirror sufficient to satisfy the scan_into /
    // update_path bound.
    impl pharos_core::GenreStore for MemStore {
        async fn upsert_genre(&self, name: &str) -> DomainResult<i64> {
            Ok(i64::from_str_radix(&pharos_core::genre_wire_id(name)[..15], 16).unwrap_or(0))
        }
        async fn link_item_genres(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
            let mut g = self
                .0
                .item_genres
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            g.insert(
                item,
                names
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
            Ok(())
        }
        async fn genres_with_counts(&self) -> DomainResult<Vec<pharos_core::GenreCount>> {
            Ok(Vec::new())
        }
        async fn item_ids_for_genre(&self, _wire_id: &str) -> DomainResult<Vec<MediaId>> {
            Ok(Vec::new())
        }
        async fn backfill_genres(&self) -> DomainResult<u64> {
            Ok(0)
        }
    }

    #[derive(Clone, Default)]
    struct FakeProber;

    impl Prober for FakeProber {
        async fn probe(&self, _path: &Path) -> DomainResult<ProbeInfo> {
            Ok(ProbeInfo {
                kind: MediaKind::Movie,
                probe: Default::default(),
            })
        }
    }

    async fn write_file(dir: &Path, name: &str, bytes: &[u8]) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&p, bytes).await.unwrap();
    }

    /// Poll `store.list()` until it reaches `n` items or `timeout` elapses.
    /// Returns true if the count was reached. Avoids a fixed sleep — the
    /// watcher's debounce + inotify latency varies in CI.
    async fn wait_for_count(store: &MemStore, n: usize, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if store.list().await.unwrap().len() >= n {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn watch_indexes_a_newly_created_file() {
        // LIB-A8 — spawn a real notify watcher on a tmpdir, create a media
        // file, and assert it lands in the store within a timeout. Skips
        // cleanly if the platform can't init a watch (e.g. inotify limits in
        // a constrained sandbox) so CI never flakes on an unsupported env.
        let td = TempDir::new().unwrap();
        let store = MemStore::default();
        // Short debounce so the test settles fast.
        let opts = WatchOptions {
            debounce: Duration::from_millis(100),
            ..Default::default()
        };
        let scanner = FsScanner::new(FakeProber);
        let handle = match spawn_watch(td.path().to_path_buf(), store.clone(), scanner, opts) {
            Ok(h) => h,
            Err(WatchError::Unsupported(msg)) => {
                eprintln!("skipping: filesystem watch unsupported here: {msg}");
                return;
            }
            Err(e) => panic!("watch failed to start: {e}"),
        };

        // Give the watcher a beat to register before we touch the tree.
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_file(td.path(), "movie.mkv", b"hello-watch").await;

        let ok = wait_for_count(&store, 1, Duration::from_secs(5)).await;
        assert!(ok, "created file should be indexed by the watcher");

        let expected_id = crate::fs::stable_id(&td.path().join("movie.mkv"));
        assert!(
            store.get(expected_id).await.is_ok(),
            "the indexed row carries the path-derived id"
        );

        drop(handle); // stop the watch task
    }

    #[tokio::test]
    async fn watch_emits_added_delta_on_update_channel() {
        // LIB-A8 — a created file's ScanOutcome reaches the handle's updates
        // channel so A9 can broadcast the delta. Skips on unsupported FS.
        let td = TempDir::new().unwrap();
        let store = MemStore::default();
        let opts = WatchOptions {
            debounce: Duration::from_millis(100),
            ..Default::default()
        };
        let mut handle = match spawn_watch(
            td.path().to_path_buf(),
            store.clone(),
            FsScanner::new(FakeProber),
            opts,
        ) {
            Ok(h) => h,
            Err(WatchError::Unsupported(_)) => return,
            Err(e) => panic!("watch failed to start: {e}"),
        };

        tokio::time::sleep(Duration::from_millis(50)).await;
        write_file(td.path(), "clip.mkv", b"delta-bytes").await;

        let expected_id = crate::fs::stable_id(&td.path().join("clip.mkv"));
        // Drain outcomes until we see our added id or time out.
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(outcome) = handle.updates.recv().await {
                if outcome.added.contains(&expected_id) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            got,
            "the created file's added-id should arrive on the channel"
        );
    }

    #[tokio::test]
    async fn watch_ignores_non_media_files() {
        // LIB-A8 — a non-media file (wrong extension) must not be indexed:
        // the recursive watch fires events for every file, but only
        // recognised extensions get probed + stored.
        let td = TempDir::new().unwrap();
        let store = MemStore::default();
        let opts = WatchOptions {
            debounce: Duration::from_millis(100),
            ..Default::default()
        };
        let handle = match spawn_watch(
            td.path().to_path_buf(),
            store.clone(),
            FsScanner::new(FakeProber),
            opts,
        ) {
            Ok(h) => h,
            Err(WatchError::Unsupported(_)) => return,
            Err(e) => panic!("watch failed to start: {e}"),
        };

        tokio::time::sleep(Duration::from_millis(50)).await;
        write_file(td.path(), "notes.txt", b"not media").await;
        write_file(td.path(), "real.mkv", b"is media").await;

        // The media file lands; the txt never does.
        assert!(
            wait_for_count(&store, 1, Duration::from_secs(5)).await,
            "the media file is indexed"
        );
        // Settle a little longer, then assert exactly one row (txt excluded).
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            store.list().await.unwrap().len(),
            1,
            "the non-media file must not be indexed"
        );
        drop(handle);
    }
}
