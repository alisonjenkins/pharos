//! Recursive media filesystem scan. Generic over `Prober` (V12).
//! Walk lives in `spawn_blocking` — never parks async runtime (V5).

use futures_util::stream::StreamExt;
use pharos_core::{
    AlternateMediaSource, DomainError, DomainResult, Fingerprint, MediaId, MediaItem, MediaKind,
    MediaStore, Prober, ScanOutcome, Scanner, SeriesInfo,
};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "mov", "avi", "webm", "m4v", "flac", "mp3", "opus", "m4a", "ogg", "wav",
];

/// SIMD-accelerated stable ID for a path. xxh3_64 hashes UTF-8 bytes,
/// then masks to 63 bits so the value always survives the
/// `u64 -> i64` conversion the sqlite store does on insert. (Half of
/// real xxh3_64 outputs exceed i64::MAX; without the mask roughly
/// half the library hits a silent "conflict" on import.) Keyspace
/// stays 2^63, which still puts collisions out of reach for any
/// realistic library size.
pub fn stable_id(path: &Path) -> u64 {
    xxh3_64(path.to_string_lossy().as_bytes()) & 0x7FFFFFFFFFFFFFFF
}

/// LIB-A2 — filesystem mtime as unix-seconds for the incremental
/// scan-state signature. Falls back to `0` when the platform doesn't
/// expose a modified time or it predates the unix epoch; `0` is the
/// same "no signature yet, must re-probe" sentinel the store uses, so a
/// degenerate mtime simply forces a probe rather than skipping wrongly.
pub(crate) fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// LIB-A5 — hard ceiling on the parallel-probe fan-out. The default
/// degree is `available_parallelism()` clamped to this; a single library
/// scan shouldn't spawn dozens of concurrent ffprobe forks / libav jobs
/// and starve the rest of the server (and disk seek thrash hurts past a
/// point anyway). Callers can still override via [`FsScanner::with_probe_concurrency`].
const MAX_PROBE_CONCURRENCY: usize = 8;

/// LIB-A5 — default probe fan-out: available CPU parallelism, clamped to
/// `[1, MAX_PROBE_CONCURRENCY]`. Falls back to 1 if the platform can't
/// report parallelism.
fn default_probe_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, MAX_PROBE_CONCURRENCY)
}

/// LIB-A8 — outcome of a single-path incremental update (one watch event).
/// The watcher maps this onto the same `added`/`updated`/`removed` delta
/// broadcast `scan_into` produces over a whole tree (A4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathUpdate {
    /// A brand-new row was inserted for this path.
    Added(MediaId),
    /// An existing row was re-probed (signature changed) or rebound (move).
    Updated(MediaId),
    /// Nothing changed — unchanged signature, idempotent move, or a probe
    /// failure that was logged + skipped (V6).
    Skipped,
}

#[derive(Debug, Clone)]
pub struct FsScanner<P: Prober> {
    prober: P,
    extensions: HashSet<String>,
    /// P43 — inter-probe pause in milliseconds. Zero (default) keeps
    /// the original full-throttle behaviour the CLI scan ships with.
    rate_limit: std::time::Duration,
    /// LIB-A5 — bounded probe fan-out. The directory walk stays in
    /// `spawn_blocking` (V5) and store writes stay serialised (V10 —
    /// sqlite single-writer), but the per-file probe (the expensive,
    /// IO/CPU-bound step) runs concurrently with this degree.
    probe_concurrency: usize,
}

impl<P: Prober> FsScanner<P> {
    pub fn new(prober: P) -> Self {
        Self {
            prober,
            extensions: DEFAULT_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            rate_limit: std::time::Duration::ZERO,
            probe_concurrency: default_probe_concurrency(),
        }
    }

    pub fn with_extensions(prober: P, exts: impl IntoIterator<Item = String>) -> Self {
        Self {
            prober,
            extensions: exts.into_iter().collect(),
            rate_limit: std::time::Duration::ZERO,
            probe_concurrency: default_probe_concurrency(),
        }
    }

    /// LIB-A8 — snapshot of the recognised-extension set, for a watcher that
    /// needs to filter raw fs events (which arrive for every file in the
    /// tree, media or not) down to the files this scanner would index.
    pub fn extensions_snapshot(&self) -> HashSet<String> {
        self.extensions.clone()
    }

    /// P43 — apply a per-probe rate-limit. `0` disables. Used by the
    /// `/Library/Refresh` background path so a re-scan of a large
    /// library doesn't saturate ffmpeg + disk during active playback.
    pub fn with_rate_limit_ms(mut self, ms: u64) -> Self {
        self.rate_limit = std::time::Duration::from_millis(ms);
        self
    }

    /// LIB-A5 — override the bounded probe fan-out. `0` is coerced to `1`
    /// (a degree of zero would stall the stream forever). Values above
    /// [`MAX_PROBE_CONCURRENCY`] are clamped down. Mainly a test seam —
    /// the default (`available_parallelism`, clamped) is what production
    /// callers use.
    pub fn with_probe_concurrency(mut self, degree: usize) -> Self {
        self.probe_concurrency = degree.clamp(1, MAX_PROBE_CONCURRENCY);
        self
    }

    /// Scan and push items into the given store. Streaming variant — avoids
    /// holding the entire library in memory. V10 atomicity holds per `put`.
    ///
    /// LIB-A2 — incremental. Opens a `scan_runs` token via
    /// [`MediaStore::begin_scan`], then for each primary stats the file
    /// (cheap) and compares its `(mtime, size)` against the persisted
    /// signature read with [`MediaStore::scan_state`]. When unchanged the
    /// expensive probe is skipped — the row is only re-stamped via
    /// [`MediaStore::mark_seen`] so the mark-and-sweep token stays current
    /// (A3 consumes it). A first-ever scan (no stored signature) behaves
    /// exactly as before: every file is probed + put.
    ///
    /// LIB-A4 — returns a structured [`ScanOutcome`] instead of a bare count:
    /// `added` (newly inserted ids), `updated` (re-probed existing ids),
    /// `removed` (swept ids), and `skipped` (unchanged-file count). Callers
    /// broadcast the added/removed deltas to connected `/socket` clients and
    /// print richer CLI summaries. The legacy probed+stored count is still
    /// available via [`ScanOutcome::probed`].
    ///
    /// LIB-A3 — deletion reconciliation. Every primary observed this run
    /// (probed *or* skipped-unchanged) is `mark_seen`'d with the current
    /// `scan_id`. After the walk completes, a single root-scoped
    /// [`MediaStore::sweep_unseen`] deletes every row under this root whose
    /// `last_seen_scan_id` is not this run's id — i.e. files that vanished
    /// from disk since the previous scan. The sweep is keyed on the
    /// canonical root prefix so scanning one root never deletes another
    /// root's items (V10: the store performs a single atomic DELETE). The
    /// swept count is threaded into [`MediaStore::finish_scan`] and the
    /// deleted ids are logged at info (broadcasting deltas is A4).
    #[tracing::instrument(skip(self, store), fields(root = %root.display()))]
    pub async fn scan_into<S: MediaStore>(
        &self,
        root: &Path,
        store: &S,
    ) -> DomainResult<ScanOutcome> {
        let scan_id = store.begin_scan(root).await?;
        let paths = walk(root.to_path_buf(), self.extensions.clone()).await?;
        let groups = group_editions(paths);
        let mut outcome = ScanOutcome::default();
        let mut seen = 0i64;

        // LIB-A5 — three phases:
        //   1. stat + skip  (sequential, cheap): unchanged files cost only a
        //      stat + scan_state read, never a probe slot.
        //   2. probe        (parallel, bounded): the expensive ffprobe/libav
        //      step runs concurrently up to `probe_concurrency`.
        //   3. write        (sequential): put/mark_seen are applied on this
        //      single task so the sqlite single-writer invariant (V10) holds —
        //      probing fans out, but the pool never sees concurrent writes.
        //
        // Phase 1 — collect the groups that actually need a probe.
        struct Pending {
            primary: PathBuf,
            alts: Vec<(String, PathBuf)>,
            id: MediaId,
            sig: Option<(i64, u64)>,
            /// Content fingerprint already computed for a path-miss group.
            /// `Some` only when we had to read the file to disambiguate a
            /// move from a fresh insert; reused so we don't hash twice.
            fp: Option<Fingerprint>,
            /// `true` when a row already existed before this run (drives the
            /// added-vs-updated split once the probe lands).
            existed: bool,
        }
        let mut pending: Vec<Pending> = Vec::new();
        for (primary, alts) in groups {
            // Cheap fs stat up front: lets us skip the expensive probe when
            // the file is byte-for-byte unchanged since the last scan. A
            // stat failure (file vanished mid-scan, permission flip) just
            // falls through to the probe path, which logs + skips on error
            // (V6) — we never abort the whole scan for one file.
            let id = stable_id(&primary);
            let stat = tokio::fs::metadata(&primary).await.ok();
            let sig = stat.as_ref().map(|m| (mtime_secs(m), m.len()));

            // Did a row already exist for this id before this run? Drives the
            // added-vs-updated split in the outcome. `scan_state` returns
            // `None` for an absent row (genuinely new) or a pre-0016 row with
            // no signature yet — both are re-probed below, but only the truly
            // new ones count as `added`.
            let existing_state = store.scan_state(id).await?;
            if let Some(state) = existing_state {
                // Existing-by-path: the row is keyed on this exact path
                // (its id is stable_id(path)). Skip the probe iff the
                // signature still matches; otherwise re-probe + put.
                if let Some((mtime, size)) = sig {
                    if state.file_mtime == mtime && state.file_size == size {
                        store.mark_seen(id, scan_id, mtime, size).await?;
                        seen += 1;
                        outcome.skipped += 1;
                        continue;
                    }
                }
                pending.push(Pending {
                    primary,
                    alts,
                    id,
                    sig,
                    fp: None,
                    existed: true,
                });
                continue;
            }

            // LIB-A7 — no row keyed on this path. Before treating the file as
            // new, fingerprint its content and look for an existing row under
            // a *different* path: that distinguishes a move/rename (same
            // bytes, old path gone) from a genuine new file or a duplicate.
            // Fingerprinting is blocking IO marshalled off the reactor (V5);
            // a hash failure (vanished/permission) just falls through to the
            // probe path, which logs + skips on error (V6).
            let fp = match crate::fingerprint::fingerprint_async(&primary, None).await {
                Ok(fp) => Some(fp),
                Err(err) => {
                    tracing::warn!(
                        path = %primary.display(),
                        error = %err,
                        "fingerprint failed; treating as new (no move detection)",
                    );
                    None
                }
            };
            let cand = match fp {
                Some(fp) => store.find_by_fp(fp).await?,
                None => None,
            };
            match cand {
                // Case b — a row already carries this fingerprint *and* it is
                // this exact path (a previously-rebound / legacy-id row). The
                // row's id != stable_id(path); re-binding again would be a
                // no-op and re-inserting under `id` would duplicate it. Just
                // re-stamp the signature + fingerprint. NO insert. Idempotent:
                // this is the steady state reached after a move.
                Some(c) if c.path == primary => {
                    if let Some((mtime, size)) = sig {
                        store.mark_seen(c.id, scan_id, mtime, size).await?;
                    }
                    if let Some(fp) = fp {
                        store.set_fingerprint(c.id, fp).await?;
                    }
                    seen += 1;
                    outcome.skipped += 1;
                    continue;
                }
                // Case c vs d — the fingerprint match lives under a different
                // path. If that old path is gone from disk it is a MOVE: keep
                // the existing id (so user_data survives), rebind its path.
                // If the old path still exists it is a genuine DUPLICATE and
                // falls through to a fresh insert under `id`.
                Some(c) if !tokio::fs::try_exists(&c.path).await.unwrap_or(false) => {
                    store.rebind_path(c.id, &primary).await?;
                    if let Some((mtime, size)) = sig {
                        store.mark_seen(c.id, scan_id, mtime, size).await?;
                    }
                    if let Some(fp) = fp {
                        store.set_fingerprint(c.id, fp).await?;
                    }
                    seen += 1;
                    outcome.updated.push(c.id);
                    continue;
                }
                // Case a (cand None) or case d (cand present, old path still
                // on disk → duplicate): probe + insert a new row under `id`.
                _ => {
                    pending.push(Pending {
                        primary,
                        alts,
                        id,
                        sig,
                        fp,
                        existed: false,
                    });
                }
            }
        }

        // Phase 2 + 3 — bounded-concurrency probe stream feeding a sequential
        // write consumer. `buffer_unordered` keeps at most `probe_concurrency`
        // probes in flight; results are awaited (in completion order) and the
        // store writes applied one at a time on this task. A probe that
        // returns `None` (failure / unrecognised) is logged inside
        // `probe_with_alternates`/`probe_one` and simply produces no write
        // (V6 — one bad file never aborts the batch).
        let rate_limit = self.rate_limit;
        let mut stream = futures_util::stream::iter(pending)
            .map(|p| {
                let rl = rate_limit;
                async move {
                    // P43 — preserve the inter-probe throttle. Under
                    // parallelism this paces each probe task's start rather
                    // than serialising the whole scan; at degree 1 it matches
                    // the original sequential pause.
                    if !rl.is_zero() {
                        tokio::time::sleep(rl).await;
                    }
                    // LIB-A7 — ensure a content fingerprint is available for
                    // the row we are about to write so a *future* scan can
                    // recognise this file by content if it moves. Reuse the
                    // one already computed during path-miss disambiguation;
                    // otherwise (existing-by-path re-probe) compute it now.
                    // Hashed with `None` duration to match Phase 1 lookups.
                    let fp = match p.fp {
                        Some(fp) => Some(fp),
                        None => crate::fingerprint::fingerprint_async(&p.primary, None)
                            .await
                            .ok(),
                    };
                    let item = self.probe_with_alternates(p.primary, p.alts).await;
                    (p.id, p.sig, p.existed, fp, item)
                }
            })
            .buffer_unordered(self.probe_concurrency);

        while let Some((_id, sig, existed, fp, item)) = stream.next().await {
            let Some(item) = item else { continue };
            let item_id = item.id;
            store.put(item).await?;
            // Persist the freshly-stat'd signature so the next scan can
            // skip this file. mark_seen is an UPDATE — the put() above
            // guarantees the row exists first.
            if let Some((mtime, size)) = sig {
                store.mark_seen(item_id, scan_id, mtime, size).await?;
            }
            // LIB-A7 — stamp the content fingerprint so a later move of this
            // file is recognised by content rather than swept + re-inserted.
            if let Some(fp) = fp {
                store.set_fingerprint(item_id, fp).await?;
            }
            // A pre-existing row (signature changed) is an update; a row
            // with no prior state is a fresh insert.
            if existed {
                outcome.updated.push(item_id);
            } else {
                outcome.added.push(item_id);
            }
            seen += 1;
        }
        // LIB-A3 — mark-and-sweep. Everything still on disk under `root`
        // got mark_seen'd above; anything else with a row under this root
        // prefix vanished and is deleted in one atomic, root-scoped pass.
        // The prefix is the canonical root string the rows' `path` columns
        // were stored under, so a sibling root is never touched.
        let root_prefix = root.to_string_lossy();
        let swept = store.sweep_unseen(scan_id, &root_prefix).await?;
        let removed = swept.len();
        if removed > 0 {
            tracing::info!(scan_id, removed, ids = ?swept, "swept rows for files removed from disk");
        }
        outcome.removed = swept;
        tracing::debug!(
            scan_id,
            added = outcome.added.len(),
            updated = outcome.updated.len(),
            skipped = outcome.skipped,
            removed,
            "incremental scan complete"
        );
        store.finish_scan(scan_id, seen, removed as i64).await?;
        Ok(outcome)
    }

    /// LIB-A8 — incremental update for a *single* path, the unit a
    /// filesystem watcher delivers (one created/modified file at a time).
    /// Mirrors the per-file branch of [`scan_into`] — stat, `scan_state`
    /// skip-check, move-detect by fingerprint, probe, `put`, then `mark_seen`
    /// and a fingerprint stamp — but operates on one path with no edition
    /// grouping (a watch event names one file; the directory listing the
    /// edition matcher needs isn't available without a fresh walk).
    ///
    /// `scan_id` is an open [`MediaStore::begin_scan`] token used only to
    /// keep `mark_seen` stamps current; the watcher never sweeps off a
    /// single event so the token isn't used to reconcile deletions here.
    ///
    /// Returns which kind of change landed so the caller can broadcast the
    /// right delta. Errors only on a store failure — a probe / stat / hash
    /// failure on the one file is logged and yields [`PathUpdate::Skipped`]
    /// (V6: a bad file never aborts the watcher loop).
    pub async fn update_path<S: MediaStore>(
        &self,
        path: &Path,
        store: &S,
        scan_id: i64,
    ) -> DomainResult<PathUpdate> {
        let id = stable_id(path);
        let stat = tokio::fs::metadata(path).await.ok();
        let sig = stat.as_ref().map(|m| (mtime_secs(m), m.len()));

        // Existing-by-path row: skip when the signature still matches,
        // else re-probe.
        let existing_state = store.scan_state(id).await?;
        if let Some(state) = existing_state {
            if let Some((mtime, size)) = sig {
                if state.file_mtime == mtime && state.file_size == size {
                    store.mark_seen(id, scan_id, mtime, size).await?;
                    return Ok(PathUpdate::Skipped);
                }
            }
            // Signature changed (or unreadable) — re-probe + put.
            return self
                .probe_put_one(path, store, scan_id, sig, None, true)
                .await;
        }

        // No row keyed on this path — fingerprint + move-detect, exactly
        // like the path-miss branch of `scan_into`.
        let fp = match crate::fingerprint::fingerprint_async(path, None).await {
            Ok(fp) => Some(fp),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "watch: fingerprint failed; treating as new (no move detection)",
                );
                None
            }
        };
        let cand = match fp {
            Some(fp) => store.find_by_fp(fp).await?,
            None => None,
        };
        match cand {
            // Steady state after a move — fingerprint already bound to this
            // exact path. Re-stamp only.
            Some(c) if c.path == path => {
                if let Some((mtime, size)) = sig {
                    store.mark_seen(c.id, scan_id, mtime, size).await?;
                }
                if let Some(fp) = fp {
                    store.set_fingerprint(c.id, fp).await?;
                }
                Ok(PathUpdate::Skipped)
            }
            // Move: same bytes, old path gone → rebind id (preserve user_data).
            Some(c) if !tokio::fs::try_exists(&c.path).await.unwrap_or(false) => {
                store.rebind_path(c.id, path).await?;
                if let Some((mtime, size)) = sig {
                    store.mark_seen(c.id, scan_id, mtime, size).await?;
                }
                if let Some(fp) = fp {
                    store.set_fingerprint(c.id, fp).await?;
                }
                Ok(PathUpdate::Updated(c.id))
            }
            // New file or genuine duplicate (old path still on disk) — probe + insert.
            _ => {
                self.probe_put_one(path, store, scan_id, sig, fp, false)
                    .await
            }
        }
    }

    /// LIB-A8 — probe a single path and persist it (the create/modify tail
    /// shared by [`update_path`]). `existed` drives the added-vs-updated
    /// result; `fp` is reused when already computed during move-detect.
    async fn probe_put_one<S: MediaStore>(
        &self,
        path: &Path,
        store: &S,
        scan_id: i64,
        sig: Option<(i64, u64)>,
        fp: Option<Fingerprint>,
        existed: bool,
    ) -> DomainResult<PathUpdate> {
        if !self.rate_limit.is_zero() {
            tokio::time::sleep(self.rate_limit).await;
        }
        let fp = match fp {
            Some(fp) => Some(fp),
            None => crate::fingerprint::fingerprint_async(path, None).await.ok(),
        };
        let Some(item) = self.probe_one(path.to_path_buf()).await else {
            // V6 — probe failed, already logged in `probe_one`. No write.
            return Ok(PathUpdate::Skipped);
        };
        let item_id = item.id;
        store.put(item).await?;
        if let Some((mtime, size)) = sig {
            store.mark_seen(item_id, scan_id, mtime, size).await?;
        }
        if let Some(fp) = fp {
            store.set_fingerprint(item_id, fp).await?;
        }
        if existed {
            Ok(PathUpdate::Updated(item_id))
        } else {
            Ok(PathUpdate::Added(item_id))
        }
    }

    /// P41 — probe primary + each alternate edition sibling, then
    /// attach the alternates to the primary's `MediaProbe`. Alternates
    /// are not indexed as independent items (the edition picker on
    /// PlaybackInfo lets users pick between them).
    async fn probe_with_alternates(
        &self,
        primary: PathBuf,
        alts: Vec<(String, PathBuf)>,
    ) -> Option<MediaItem> {
        let mut item = self.probe_one(primary).await?;
        for (edition, alt_path) in alts {
            match self.prober.probe(&alt_path).await {
                Ok(info) => {
                    let mut probe = info.probe;
                    if probe.size_bytes.is_none() {
                        if let Ok(meta) = tokio::fs::metadata(&alt_path).await {
                            probe.size_bytes = Some(meta.len());
                        }
                    }
                    // Stable id suffix derived from the edition tag so
                    // URL paths survive re-scans the same way the
                    // primary's id does.
                    let id = edition_id_slug(&edition);
                    item.probe.alternate_sources.push(AlternateMediaSource {
                        id,
                        path: alt_path,
                        container: probe.container,
                        video_codec: probe.video_codec,
                        audio_codec: probe.audio_codec,
                        bitrate_bps: probe.bitrate_bps,
                        size_bytes: probe.size_bytes,
                        duration_ms: probe.duration_ms,
                        name: Some(edition),
                    });
                }
                Err(err) => {
                    tracing::warn!(
                        path = %alt_path.display(),
                        error = %err,
                        "alt edition probe failed, skipping just this alternate",
                    );
                }
            }
        }
        Some(item)
    }

    async fn probe_one(&self, path: PathBuf) -> Option<MediaItem> {
        match self.prober.probe(&path).await {
            Ok(info) => {
                let title = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                // Stat the file so MediaProbe.size_bytes is set even when
                // ffprobe didn't report `format.size` (some containers).
                let mut probe = info.probe;
                if probe.size_bytes.is_none() {
                    if let Ok(meta) = tokio::fs::metadata(&path).await {
                        probe.size_bytes = Some(meta.len());
                    }
                }
                // Promote video-kind items to Episode when the path
                // looks like a TV layout. Audio stays as classified.
                let kind = if matches!(info.kind, MediaKind::Movie) && is_episode_path(&path) {
                    MediaKind::Episode
                } else {
                    info.kind
                };
                let series = if matches!(kind, MediaKind::Episode) {
                    parse_series_info(&path)
                } else {
                    None
                };
                Some(MediaItem {
                    id: stable_id(&path),
                    path,
                    title,
                    kind,
                    probe,
                    series,
                    // Let the store-side `now_secs` populate. Passing
                    // None preserves the original `created_at` on
                    // rescan via the COALESCE in put().
                    created_at: None,
                    // LIB-C7/C8/C9 — descriptive metadata is enriched by
                    // a later EPIC D pass; the scanner emits an empty
                    // block today.
                    metadata: Default::default(),
                })
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "probe failed, skipping");
                None
            }
        }
    }
}

impl<P: Prober + Clone + 'static> Scanner for FsScanner<P> {
    #[tracing::instrument(skip(self), fields(root = %root.display()))]
    async fn scan(&self, root: &Path) -> DomainResult<Vec<MediaItem>> {
        let paths = walk(root.to_path_buf(), self.extensions.clone()).await?;
        let groups = group_editions(paths);
        let mut items = Vec::with_capacity(groups.len());
        for (primary, alts) in groups {
            if let Some(item) = self.probe_with_alternates(primary, alts).await {
                items.push(item);
            }
            if !self.rate_limit.is_zero() {
                tokio::time::sleep(self.rate_limit).await;
            }
        }
        Ok(items)
    }
}

/// P41 — known edition labels that demote a sibling file to an
/// `AlternateMediaSource` of the matching primary instead of a
/// standalone library item. Matched case-insensitively against the
/// trailing ` - Edition` portion of the file stem.
const KNOWN_EDITIONS: &[&str] = &[
    "director's cut",
    "directors cut",
    "extended",
    "extended cut",
    "extended edition",
    "theatrical",
    "theatrical cut",
    "remastered",
    "imax",
    "imax edition",
    "unrated",
    "uncut",
    "special edition",
    "criterion",
    "criterion collection",
    "original",
    "original cut",
    "redux",
    "final cut",
    "international cut",
    "ultimate edition",
    "anniversary edition",
];

/// P41 — split a file stem like `"Movie Title - Director's Cut"` into
/// its primary title + edition tag. Returns `None` when the trailing
/// segment isn't in `KNOWN_EDITIONS` so titles that legitimately
/// contain ` - ` ("Crouching Tiger, Hidden Dragon") aren't mangled.
pub fn split_edition_tag(stem: &str) -> Option<(&str, &str)> {
    let (left, right) = stem.rsplit_once(" - ")?;
    let edition = right.trim();
    if !is_known_edition(edition) {
        return None;
    }
    Some((left.trim(), edition))
}

fn is_known_edition(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    KNOWN_EDITIONS.iter().any(|e| *e == lower)
}

/// P41 — slugify an edition label into a URL-stable identifier suffix
/// for `MediaSourceInfo.Id`. Lowercase, ascii-only, `-` separator.
fn edition_id_slug(edition: &str) -> String {
    let mut s = String::with_capacity(edition.len());
    for c in edition.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    s.trim_matches('-').to_string()
}

/// P41 — group walk output into `(primary, Vec<(edition_label, alt_path)>)`
/// tuples. Files whose stem matches `Title - <known_edition>` and that
/// share a directory + a primary file (`Title.ext`) are demoted to
/// alternates of the primary. Files without a matching primary
/// remain stand-alone items (the edition tag is preserved in the
/// title).
pub(crate) fn group_editions(paths: Vec<PathBuf>) -> Vec<(PathBuf, Vec<(String, PathBuf)>)> {
    // Index primaries by (parent_dir, lowercase_title). BTreeMap so
    // iteration order is deterministic, which matters for tests +
    // for the deterministic stable_id seed.
    let mut primaries: BTreeMap<(PathBuf, String), PathBuf> = BTreeMap::new();
    let mut alternates: Vec<(PathBuf, String, PathBuf)> = Vec::new();
    let mut standalone: Vec<PathBuf> = Vec::new();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            standalone.push(path);
            continue;
        };
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
        if split_edition_tag(stem).is_some() {
            alternates.push((parent, stem.to_string(), path));
        } else {
            primaries.insert((parent.clone(), stem.to_ascii_lowercase()), path.clone());
            standalone.push(path);
        }
    }
    let mut groups: BTreeMap<PathBuf, Vec<(String, PathBuf)>> = BTreeMap::new();
    let mut orphan_alts: Vec<PathBuf> = Vec::new();
    for (parent, stem, alt_path) in alternates {
        let (title, edition) = match split_edition_tag(&stem) {
            Some(t) => t,
            None => {
                orphan_alts.push(alt_path);
                continue;
            }
        };
        let key = (parent, title.to_ascii_lowercase());
        match primaries.get(&key) {
            Some(primary) => {
                groups
                    .entry(primary.clone())
                    .or_default()
                    .push((edition.to_string(), alt_path));
            }
            None => {
                // No matching primary in the same directory — keep as
                // standalone item so the user still sees the file.
                orphan_alts.push(alt_path);
            }
        }
    }
    let mut out: Vec<(PathBuf, Vec<(String, PathBuf)>)> = Vec::new();
    for path in standalone {
        let alts = groups.remove(&path).unwrap_or_default();
        out.push((path, alts));
    }
    for path in orphan_alts {
        out.push((path, Vec::new()));
    }
    out
}

/// Heuristic: does `path` look like a TV episode?
///
/// We accept either signal:
/// - filename contains an `SxxEyy` token (case-insensitive, with any
///   non-letter separator before the `S` to avoid matching mid-word
///   IDs like "GS9E2-clip"); or
/// - any parent directory is named `Season N`, `Season NN`, `S<NN>`,
///   `Specials`, or `Season 0` (the Plex/Jellyfin layout convention).
///
/// Path-only — no probe required. Files in a "Movies/" tree never hit
/// either signal and stay Movie.
pub fn is_episode_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if has_sxxeyy_token(name) {
        return true;
    }
    for component in path.components() {
        let comp = component.as_os_str().to_string_lossy();
        if looks_like_season_dir(&comp) {
            return true;
        }
    }
    false
}

fn has_sxxeyy_token(name: &str) -> bool {
    let bytes = name.as_bytes();
    let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
    let mut i = 0;
    while i + 5 < lower.len() {
        // boundary: start or non-letter before 's'
        let at_boundary = i == 0 || !lower[i - 1].is_ascii_alphabetic();
        if at_boundary && lower[i] == b's' && lower[i + 1].is_ascii_digit() {
            // optional second season digit
            let mut j = i + 2;
            if j < lower.len() && lower[j].is_ascii_digit() {
                j += 1;
            }
            if j < lower.len() && lower[j] == b'e' {
                let mut k = j + 1;
                if k < lower.len() && lower[k].is_ascii_digit() {
                    k += 1;
                    if k < lower.len() && lower[k].is_ascii_digit() {
                        return true;
                    }
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Extract `SeriesInfo { series_name, season_number, episode_number }`
/// from a TV-layout path. Heuristic:
/// - series_name = the closest ancestor directory of `path` that is
///   *not* a "Season N" / "S01" / "Specials" / a configured media
///   root token. Falls back to the immediate parent directory name
///   when nothing else fits.
/// - season_number = parsed from a "Season N" / "S<NN>" parent dir
///   if present, or from the `SxxEyy` token in the filename.
/// - episode_number = parsed from the `SxxEyy` token in the filename.
///
/// Returns `None` when `path` has no parent — pathological case.
pub fn parse_series_info(path: &Path) -> Option<SeriesInfo> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let (filename_season, episode) = parse_sxxeyy(name);

    // Walk parents from closest to farthest.
    let mut parents: Vec<&str> = path
        .ancestors()
        .skip(1)
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
        .collect();

    let mut season_from_dir: Option<u32> = None;
    let mut series_name: Option<String> = None;

    for parent in parents.drain(..) {
        if let Some(n) = parse_season_dir(parent) {
            season_from_dir = season_from_dir.or(Some(n));
            continue;
        }
        if parent.eq_ignore_ascii_case("specials") {
            season_from_dir = season_from_dir.or(Some(0));
            continue;
        }
        // First non-season ancestor wins as the series name.
        if series_name.is_none() {
            series_name = Some(parent.to_string());
            break;
        }
    }

    let series_name = series_name?;
    let season_number = season_from_dir.or(filename_season);
    Some(SeriesInfo {
        series_name,
        season_number,
        episode_number: episode,
    })
}

/// Return the (season, episode) numbers when `name` carries an
/// `SxxEyy` token at any letter-boundary. `None` if absent.
fn parse_sxxeyy(name: &str) -> (Option<u32>, Option<u32>) {
    let lower: Vec<u8> = name.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let mut i = 0;
    while i + 5 < lower.len() {
        let at_boundary = i == 0 || !lower[i - 1].is_ascii_alphabetic();
        if at_boundary && lower[i] == b's' && lower[i + 1].is_ascii_digit() {
            // collect season digits
            let s_start = i + 1;
            let mut s_end = s_start + 1;
            while s_end < lower.len() && lower[s_end].is_ascii_digit() {
                s_end += 1;
            }
            if s_end < lower.len() && lower[s_end] == b'e' {
                let e_start = s_end + 1;
                let mut e_end = e_start;
                while e_end < lower.len() && lower[e_end].is_ascii_digit() {
                    e_end += 1;
                }
                if e_end > e_start {
                    let season = std::str::from_utf8(&lower[s_start..s_end])
                        .ok()
                        .and_then(|s| s.parse().ok());
                    let episode = std::str::from_utf8(&lower[e_start..e_end])
                        .ok()
                        .and_then(|s| s.parse().ok());
                    return (season, episode);
                }
            }
        }
        i += 1;
    }
    (None, None)
}

/// Parse a "Season N" / "Season NN" / "S01" / "S1" directory name → N.
fn parse_season_dir(name: &str) -> Option<u32> {
    let n = name.trim();
    if let Some(rest) = n.to_ascii_lowercase().strip_prefix("season ") {
        return rest.trim().parse().ok();
    }
    let lower = n.to_ascii_lowercase();
    if lower.starts_with('s')
        && lower.len() >= 2
        && lower.len() <= 4
        && lower[1..].chars().all(|c| c.is_ascii_digit())
    {
        return lower[1..].parse().ok();
    }
    None
}

fn looks_like_season_dir(name: &str) -> bool {
    let n = name.trim();
    if n.eq_ignore_ascii_case("specials") {
        return true;
    }
    let lower = n.to_ascii_lowercase();
    // "Season 1", "Season 02", "Season 10"
    if let Some(rest) = lower.strip_prefix("season ") {
        return rest.trim().chars().all(|c| c.is_ascii_digit()) && !rest.trim().is_empty();
    }
    // Compact "S01", "S1" — only when whole component is that form so
    // we don't grab a file named "S01E03.mkv" (handled by SxxEyy path).
    if lower.starts_with('s')
        && lower.len() >= 2
        && lower.len() <= 4
        && lower[1..].chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    false
}

/// Recursive walk inside `spawn_blocking`. Returns paths of files whose
/// lowercased extension is in `exts`.
async fn walk(root: PathBuf, exts: HashSet<String>) -> DomainResult<Vec<PathBuf>> {
    tokio::task::spawn_blocking(move || -> DomainResult<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let e = entry.map_err(|err| DomainError::Backend(err.to_string()))?;
            if !e.file_type().is_file() {
                continue;
            }
            let lower = e
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase());
            if let Some(ext) = lower {
                if exts.contains(&ext) {
                    out.push(e.into_path());
                }
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| DomainError::Backend(format!("walk join: {e}")))?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{MediaId, MediaKind, ProbeInfo, ScanState};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// In-memory `MediaStore` for scanner tests — mirrors the canonical
    /// `MemStore` in `pharos-core/src/tests.rs` so the incremental
    /// `scan_into` path (begin_scan / scan_state / mark_seen /
    /// finish_scan) can be exercised without pulling in the sqlx store.
    #[derive(Default)]
    struct MemStore {
        inner: Mutex<HashMap<MediaId, MediaItem>>,
        states: Mutex<HashMap<MediaId, ScanState>>,
        fps: Mutex<HashMap<MediaId, pharos_core::Fingerprint>>,
        next_scan_id: AtomicI64,
    }

    impl MediaStore for MemStore {
        async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
            self.inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .cloned()
                .ok_or(DomainError::NotFound(id))
        }
        async fn put(&self, item: MediaItem) -> DomainResult<()> {
            self.inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(item.id, item);
            Ok(())
        }
        async fn list(&self) -> DomainResult<Vec<MediaItem>> {
            Ok(self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .values()
                .cloned()
                .collect())
        }
        async fn scan_state(&self, id: MediaId) -> DomainResult<Option<ScanState>> {
            Ok(self
                .states
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .copied())
        }
        async fn begin_scan(&self, _root: &Path) -> DomainResult<i64> {
            Ok(self.next_scan_id.fetch_add(1, Ordering::SeqCst) + 1)
        }
        async fn mark_seen(
            &self,
            id: MediaId,
            scan_id: i64,
            mtime: i64,
            size: u64,
        ) -> DomainResult<()> {
            // Mirror the store: mark_seen is an UPDATE — only stamp rows
            // that already exist (the scanner put()s before marking).
            if !self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .contains_key(&id)
            {
                return Ok(());
            }
            self.states
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(
                    id,
                    ScanState {
                        last_scanned: scan_id, // non-zero so "seen" is observable
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
            let mut inner = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let states = self
                .states
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            // Mirror the production store's path-boundary semantics: only
            // items strictly under `root_prefix` (separator boundary), never
            // a sibling sharing a string prefix.
            let base = root_prefix.strip_suffix('/').unwrap_or(root_prefix);
            let under_root = format!("{base}/");
            let doomed: Vec<MediaId> = inner
                .iter()
                .filter(|(id, item)| {
                    item.path.to_string_lossy().starts_with(&under_root)
                        && states.get(*id).map(|s| s.last_seen_scan_id) != Some(scan_id)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in &doomed {
                inner.remove(id);
            }
            Ok(doomed)
        }
        async fn finish_scan(
            &self,
            _scan_id: i64,
            _items_seen: i64,
            _items_swept: i64,
        ) -> DomainResult<()> {
            Ok(())
        }
        async fn find_by_fp(
            &self,
            fp: pharos_core::Fingerprint,
        ) -> DomainResult<Option<MediaItem>> {
            let fps = self
                .fps
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let inner = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut matches: Vec<MediaId> = fps
                .iter()
                .filter(|(_, v)| **v == fp)
                .map(|(id, _)| *id)
                .collect();
            matches.sort_unstable();
            Ok(matches.into_iter().find_map(|id| inner.get(&id).cloned()))
        }
        async fn set_fingerprint(
            &self,
            id: MediaId,
            fp: pharos_core::Fingerprint,
        ) -> DomainResult<()> {
            // Mirror the store: UPDATE-only, no-op when the row is absent.
            if !self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .contains_key(&id)
            {
                return Ok(());
            }
            self.fps
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(id, fp);
            Ok(())
        }
        async fn rebind_path(&self, id: MediaId, new_path: &Path) -> DomainResult<()> {
            // Mirror the store: UPDATE-only, no-op when the row is absent.
            // Keeps the id (and any associated user_data) intact.
            if let Some(item) = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get_mut(&id)
            {
                item.path = new_path.to_path_buf();
            }
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeProber {
        calls: Arc<AtomicUsize>,
        force_fail_for: Option<String>,
    }

    impl Prober for FakeProber {
        async fn probe(&self, path: &Path) -> DomainResult<ProbeInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(needle) = &self.force_fail_for {
                if path.to_string_lossy().contains(needle.as_str()) {
                    return Err(DomainError::Backend("forced fail".into()));
                }
            }
            let kind = match path.extension().and_then(|s| s.to_str()) {
                Some("flac") | Some("mp3") | Some("opus") | Some("m4a") | Some("ogg")
                | Some("wav") => MediaKind::Audio,
                _ => MediaKind::Movie,
            };
            Ok(ProbeInfo {
                kind,
                probe: pharos_core::MediaProbe {
                    genre: self.genre.clone(),
                    ..Default::default()
                },
            })
        }
    }

    async fn touch(dir: &Path, name: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&p, b"").await.unwrap();
    }

    /// Write `bytes` to `dir/name`, creating parents. Used by the
    /// incremental tests where the file *size* (and thus the scan-state
    /// signature) must change deterministically without depending on
    /// `filetime` (not a dep).
    async fn write_file(dir: &Path, name: &str, bytes: &[u8]) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&p, bytes).await.unwrap();
    }

    #[tokio::test]
    async fn finds_recognized_extensions_and_skips_others() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "movie.mkv").await;
        touch(td.path(), "song.flac").await;
        touch(td.path(), "notes.txt").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        let titles: Vec<_> = items.iter().map(|i| i.title.clone()).collect();
        assert_eq!(items.len(), 2, "got {titles:?}");
        let kinds: HashSet<MediaKind> = items.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&MediaKind::Movie));
        assert!(kinds.contains(&MediaKind::Audio));
    }

    #[tokio::test]
    async fn empty_dir_returns_empty() {
        let td = TempDir::new().unwrap();
        let s = FsScanner::new(FakeProber::default());
        assert!(s.scan(td.path()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn recurses_subdirs() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "top.mkv").await;
        touch(td.path(), "show/season1/ep1.mkv").await;
        touch(td.path(), "show/season1/ep2.mkv").await;
        touch(td.path(), "music/album/track.flac").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 4);
    }

    #[tokio::test]
    async fn probe_failure_is_logged_and_skipped() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "good.mkv").await;
        touch(td.path(), "bad.mkv").await;
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            force_fail_for: Some("bad".into()),
            genre: None,
        };
        let s = FsScanner::new(prober.clone());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "good");
        assert_eq!(prober.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn incremental_second_scan_probes_zero_unchanged_files() {
        // LIB-A2 — first scan probes every file; a second scan with no
        // filesystem change probes NONE of them. The probe counter is the
        // load-bearing assertion (the backlog's "second scan probes 0
        // unchanged" requirement).
        let td = TempDir::new().unwrap();
        write_file(td.path(), "movie.mkv", b"aaaa").await;
        write_file(td.path(), "song.flac", b"bbbbbb").await;
        write_file(td.path(), "show/s1/ep1.mkv", b"cc").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();

        let first = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(first.probed(), 3, "first scan stores all three");
        // LIB-A4 — a fresh scan reports every file as added, nothing else.
        assert_eq!(first.added.len(), 3, "all three are added on first scan");
        assert!(first.updated.is_empty(), "nothing updated on first scan");
        assert!(first.removed.is_empty(), "nothing removed on first scan");
        assert_eq!(first.skipped, 0, "nothing skipped on first scan");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            3,
            "first scan probes all three"
        );

        // Second scan, nothing touched on disk.
        let before = prober.calls.load(Ordering::SeqCst);
        let second = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(
            second.probed(),
            0,
            "nothing re-probed/stored on unchanged rescan"
        );
        // LIB-A4 — an unchanged rescan reports everything as skipped.
        assert!(second.added.is_empty(), "nothing added on unchanged rescan");
        assert!(
            second.updated.is_empty(),
            "nothing updated on unchanged rescan"
        );
        assert!(
            second.removed.is_empty(),
            "nothing removed on unchanged rescan"
        );
        assert_eq!(second.skipped, 3, "all three skipped on unchanged rescan");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst) - before,
            0,
            "unchanged files are not re-probed"
        );
        // Rows are still present (skipped != deleted).
        assert_eq!(store.list().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn incremental_reprobes_only_the_changed_file() {
        // LIB-A2 — change exactly one file (size delta via rewrite) and
        // assert exactly one re-probe on the next scan.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.mkv", b"aaaa").await;
        write_file(td.path(), "b.mkv", b"bbbb").await;
        write_file(td.path(), "c.mkv", b"cccc").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();

        s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(prober.calls.load(Ordering::SeqCst), 3);

        // Mutate just b.mkv: different byte length => different stat size
        // => signature mismatch => re-probe. a/c are byte-identical.
        write_file(td.path(), "b.mkv", b"bbbbbbbb").await;

        let changed_id = stable_id(&td.path().join("b.mkv"));
        let before = prober.calls.load(Ordering::SeqCst);
        let n = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(n.probed(), 1, "exactly one item re-stored");
        // LIB-A4 — a content change is reported as an update (not an add),
        // and the updated id is the file that actually changed.
        assert!(n.added.is_empty(), "changed file is an update, not an add");
        assert_eq!(n.updated, vec![changed_id], "the changed file is updated");
        assert!(n.removed.is_empty(), "nothing removed");
        assert_eq!(n.skipped, 2, "the two unchanged files are skipped");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst) - before,
            1,
            "exactly one file re-probed"
        );
    }

    #[tokio::test]
    async fn incremental_reprobes_on_mtime_change_same_size() {
        // LIB-A2 — same size but a newer mtime must still re-probe (a file
        // overwritten in place with equal length). Uses the std
        // `File::set_modified` API so no `filetime` dep is needed.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "x.mkv", b"hello").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();

        s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(prober.calls.load(Ordering::SeqCst), 1);

        // Bump mtime forward by an hour, keep the byte length identical.
        let p = td.path().join("x.mkv");
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        f.set_modified(bumped).unwrap();
        drop(f);

        let before = prober.calls.load(Ordering::SeqCst);
        let n = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(n.probed(), 1, "mtime-only change still re-stores");
        // LIB-A4 — an in-place mtime bump is an update of the existing row.
        assert!(n.added.is_empty());
        assert_eq!(n.updated.len(), 1, "mtime change reported as an update");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst) - before,
            1,
            "mtime change re-probes even at equal size"
        );
    }

    #[tokio::test]
    async fn deletion_reconciliation_sweeps_removed_files() {
        // LIB-A3 — a file deleted from disk between scans has its row
        // swept on the next scan; the surviving file's row remains; and a
        // file under a DIFFERENT root is untouched by the first root's
        // sweep (root-scoped delete).
        let roota = TempDir::new().unwrap();
        let rootb = TempDir::new().unwrap();
        write_file(roota.path(), "keep.mkv", b"keep").await;
        write_file(roota.path(), "gone.mkv", b"gone").await;
        write_file(rootb.path(), "other.mkv", b"other").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();

        // First scan of each root: every file probed + stored.
        s.scan_into(roota.path(), &store).await.unwrap();
        s.scan_into(rootb.path(), &store).await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 3);

        let keep_id = stable_id(&roota.path().join("keep.mkv"));
        let gone_id = stable_id(&roota.path().join("gone.mkv"));
        let other_id = stable_id(&rootb.path().join("other.mkv"));

        // Delete one file from rootA, then rescan rootA only.
        tokio::fs::remove_file(roota.path().join("gone.mkv"))
            .await
            .unwrap();
        let outcome = s.scan_into(roota.path(), &store).await.unwrap();

        // LIB-A4 — the swept file surfaces in the outcome's `removed` delta
        // (the id the broadcast layer relays as ItemsRemoved); keep.mkv is
        // unchanged so it's skipped, not removed.
        assert_eq!(
            outcome.removed,
            vec![gone_id],
            "deleted file reported removed"
        );
        assert!(outcome.added.is_empty(), "nothing added on a delete rescan");
        assert!(
            outcome.updated.is_empty(),
            "nothing updated on a delete rescan"
        );
        assert_eq!(outcome.skipped, 1, "the surviving file is skipped");

        // gone.mkv swept; keep.mkv survives; rootB's file untouched.
        assert!(store.get(keep_id).await.is_ok(), "surviving file remains");
        match store.get(gone_id).await {
            Err(DomainError::NotFound(id)) if id == gone_id => {}
            other => panic!("deleted file should be swept, got {other:?}"),
        }
        assert!(
            store.get(other_id).await.is_ok(),
            "sibling root must be untouched by rootA sweep"
        );
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn move_preserves_id() {
        // LIB-A7 — a file moved/renamed on disk keeps its existing
        // media_items.id (so user_data / watch history survives) rather than
        // being swept + re-inserted under the new path's id.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "old/movie.mkv", b"unique-content-aaaa").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();

        let first = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(first.added.len(), 1, "the file imports once");
        let old_id = stable_id(&td.path().join("old/movie.mkv"));
        assert!(store.get(old_id).await.is_ok(), "row under old path id");
        let probes_after_first = prober.calls.load(Ordering::SeqCst);

        // Move the file to a new path (same bytes => same fingerprint, new
        // path => new stable_id). Remove old, write identical content at new.
        tokio::fs::remove_file(td.path().join("old/movie.mkv"))
            .await
            .unwrap();
        write_file(td.path(), "new/film.mkv", b"unique-content-aaaa").await;
        let new_path = td.path().join("new/film.mkv");
        let new_id = stable_id(&new_path);
        assert_ne!(old_id, new_id, "the path-derived id changed");

        let outcome = s.scan_into(td.path(), &store).await.unwrap();

        // Exactly one row, still under the ORIGINAL id, path repointed.
        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 1, "no duplicate row created by the move");
        let row = store.get(old_id).await.expect("id preserved across move");
        assert_eq!(row.path, new_path, "path rebound to the new location");
        match store.get(new_id).await {
            Err(DomainError::NotFound(_)) => {}
            other => panic!("no new-id row should exist, got {other:?}"),
        }
        // The move is recognised by content — no fresh insert, no probe of
        // the moved file, and nothing left orphaned for the sweep.
        assert!(outcome.added.is_empty(), "a move is not an add");
        assert_eq!(outcome.updated, vec![old_id], "move reported as an update");
        assert!(outcome.removed.is_empty(), "the old path is NOT swept");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            probes_after_first,
            "a move re-binds without re-probing"
        );
    }

    #[tokio::test]
    async fn move_idempotent_on_rescan() {
        // LIB-A7 — after a move+rebind, a SUBSEQUENT scan must NOT create a
        // duplicate. The fingerprint now matches a row whose path is this
        // exact path (case b) => mark_seen only.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "old/movie.mkv", b"idempotent-bytes-zz").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();

        s.scan_into(td.path(), &store).await.unwrap();
        let old_id = stable_id(&td.path().join("old/movie.mkv"));

        tokio::fs::remove_file(td.path().join("old/movie.mkv"))
            .await
            .unwrap();
        write_file(td.path(), "new/film.mkv", b"idempotent-bytes-zz").await;

        // First rescan: the move.
        s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 1);

        // Second rescan, nothing changed on disk: still exactly one row, same
        // id, no new insert (the bug this guards against is a duplicate that
        // only appears on the scan AFTER a move).
        let again = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(
            store.list().await.unwrap().len(),
            1,
            "no duplicate on the post-move rescan"
        );
        assert!(store.get(old_id).await.is_ok(), "id still preserved");
        assert!(again.added.is_empty(), "idempotent rescan adds nothing");
        assert!(
            again.removed.is_empty(),
            "idempotent rescan removes nothing"
        );
    }

    #[tokio::test]
    async fn duplicate_file_creates_second_item() {
        // LIB-A7 — when the original file is STILL present and an identical
        // copy appears under a new path, that is a genuine duplicate: a new
        // row is inserted (not a rebind), so both files are tracked.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a/movie.mkv", b"dup-content-12345").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();

        s.scan_into(td.path(), &store).await.unwrap();
        let id_a = stable_id(&td.path().join("a/movie.mkv"));

        // Copy (original kept on disk) — identical bytes, different path.
        write_file(td.path(), "b/movie.mkv", b"dup-content-12345").await;
        let id_b = stable_id(&td.path().join("b/movie.mkv"));
        assert_ne!(id_a, id_b);

        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 2, "duplicate copy creates a second row");
        assert!(store.get(id_a).await.is_ok(), "original row remains");
        assert!(store.get(id_b).await.is_ok(), "copy gets its own row");
        assert_eq!(outcome.added, vec![id_b], "the copy is reported as added");
    }

    #[tokio::test]
    async fn genuine_new_still_inserts() {
        // LIB-A7 — a file whose content matches NOTHING in the store (no
        // fingerprint hit) is a plain new import: probe + insert under its
        // own path id, exactly as before A7.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "first.mkv", b"first-unique-aaa").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();
        s.scan_into(td.path(), &store).await.unwrap();

        // Add a second, content-distinct file and rescan.
        write_file(td.path(), "second.mkv", b"second-unique-bbb").await;
        let second_id = stable_id(&td.path().join("second.mkv"));
        let outcome = s.scan_into(td.path(), &store).await.unwrap();

        assert_eq!(outcome.added, vec![second_id], "new file imported as added");
        assert_eq!(store.list().await.unwrap().len(), 2);
        assert!(store.get(second_id).await.is_ok());
    }

    /// LIB-A5 — prober that records peak concurrent in-flight probes. Each
    /// probe bumps an `in_flight` counter, sleeps briefly so overlap is
    /// observable, tracks the running max, then decrements. Lets a test
    /// assert the bounded-concurrency stream actually overlaps probes (and
    /// never exceeds the configured degree).
    #[derive(Clone, Default)]
    struct ConcurrencyProber {
        in_flight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl Prober for ConcurrencyProber {
        async fn probe(&self, _path: &Path) -> DomainResult<ProbeInfo> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            // Lift the recorded peak to `now` if higher (CAS loop).
            let mut cur = self.peak.load(Ordering::SeqCst);
            while now > cur {
                match self
                    .peak
                    .compare_exchange(cur, now, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => break,
                    Err(observed) => cur = observed,
                }
            }
            // Hold the slot long enough for siblings to overlap.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(ProbeInfo {
                kind: MediaKind::Movie,
                probe: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn parallel_scan_imports_same_item_set_as_sequential() {
        // LIB-A5 — the parallel probe path must import exactly the same set
        // of items as a single-degree scan. Ordering of the ScanOutcome vecs
        // may differ under concurrency, so compare as sets of ids.
        let names = [
            "a.mkv", "b.mkv", "c.mkv", "d.mkv", "e.mkv", "f.mkv", "g.mkv", "h.mkv",
        ];

        // Sequential baseline (degree 1).
        let seq_td = TempDir::new().unwrap();
        for (i, n) in names.iter().enumerate() {
            write_file(seq_td.path(), n, &vec![b'x'; i + 1]).await;
        }
        let seq_store = MemStore::default();
        let seq_scanner = FsScanner::new(FakeProber::default()).with_probe_concurrency(1);
        let seq = seq_scanner
            .scan_into(seq_td.path(), &seq_store)
            .await
            .unwrap();
        let seq_ids: HashSet<MediaId> = seq.added.iter().copied().collect();

        // Parallel (degree 8) over an identically-named tree.
        let par_td = TempDir::new().unwrap();
        for (i, n) in names.iter().enumerate() {
            write_file(par_td.path(), n, &vec![b'x'; i + 1]).await;
        }
        let par_store = MemStore::default();
        let par_scanner = FsScanner::new(FakeProber::default()).with_probe_concurrency(8);
        let par = par_scanner
            .scan_into(par_td.path(), &par_store)
            .await
            .unwrap();
        let par_ids: HashSet<MediaId> = par.added.iter().copied().collect();

        // Same count + same per-file stable ids (ids are path-derived; the
        // two roots differ, so compare counts + the stored item *paths*' base
        // names instead of raw ids).
        assert_eq!(seq.added.len(), names.len());
        assert_eq!(par.added.len(), names.len());
        assert_eq!(seq_ids.len(), names.len(), "no duplicate adds, sequential");
        assert_eq!(par_ids.len(), names.len(), "no duplicate adds, parallel");

        let basename_set = |items: Vec<MediaItem>| -> HashSet<String> {
            items
                .into_iter()
                .map(|i| {
                    i.path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        };
        let seq_set = basename_set(seq_store.list().await.unwrap());
        let par_set = basename_set(par_store.list().await.unwrap());
        assert_eq!(seq_set, par_set, "parallel scan imports the same file set");
        assert_eq!(seq_set.len(), names.len());
    }

    #[tokio::test]
    async fn parallel_scan_isolates_a_single_probe_failure() {
        // LIB-A5 + V6 — under concurrency, one failing probe must not abort
        // the batch: every other file is still imported. Reuses the
        // FakeProber's `force_fail_for` path-needle.
        let td = TempDir::new().unwrap();
        for n in ["one.mkv", "two.mkv", "bad.mkv", "four.mkv", "five.mkv"] {
            write_file(td.path(), n, n.as_bytes()).await;
        }
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            force_fail_for: Some("bad".into()),
            genre: None,
        };
        let s = FsScanner::new(prober.clone()).with_probe_concurrency(4);
        let store = MemStore::default();

        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        // 5 probed, 1 failed => 4 imported.
        assert_eq!(outcome.added.len(), 4, "all non-failing files imported");
        assert_eq!(prober.calls.load(Ordering::SeqCst), 5, "every file probed");

        let names: HashSet<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|i| i.path.file_name()?.to_str().map(str::to_string))
            .collect();
        assert!(
            !names.contains("bad.mkv"),
            "the failing file is not imported"
        );
        for ok in ["one.mkv", "two.mkv", "four.mkv", "five.mkv"] {
            assert!(names.contains(ok), "{ok} should be imported");
        }
    }

    #[tokio::test]
    async fn parallel_scan_respects_concurrency_cap() {
        // LIB-A5 — the bounded stream must (a) overlap probes when degree > 1
        // and (b) never exceed the configured degree. With degree 3 over 9
        // files the observed peak must land in [2, 3].
        let td = TempDir::new().unwrap();
        for i in 0..9 {
            write_file(td.path(), &format!("f{i}.mkv"), &[b'x'; 1]).await;
        }
        let prober = ConcurrencyProber::default();
        let s = FsScanner::new(prober.clone()).with_probe_concurrency(3);
        let store = MemStore::default();

        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 9, "all nine imported");

        let peak = prober.peak.load(Ordering::SeqCst);
        assert!(
            peak >= 2,
            "probes should overlap under concurrency (peak {peak})"
        );
        assert!(
            peak <= 3,
            "concurrency cap of 3 must be respected (peak {peak})"
        );
        // The in-flight counter must have drained back to zero.
        assert_eq!(prober.in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn probe_concurrency_zero_is_coerced_to_one() {
        // LIB-A5 — a degree of 0 would stall buffer_unordered forever; the
        // builder clamps it to 1. The scan must still complete.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "solo.mkv", b"x").await;
        let s = FsScanner::new(FakeProber::default()).with_probe_concurrency(0);
        let store = MemStore::default();
        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 1);
    }

    #[tokio::test]
    async fn promotes_to_episode_when_path_matches_sxxeyy() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Show/Season 1/Show.S01E02.mkv").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].kind, MediaKind::Episode));
    }

    #[tokio::test]
    async fn movies_path_stays_movie() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Movies/Big Buck Bunny (2008).mkv").await;
        let s = FsScanner::new(FakeProber::default());
        let items = s.scan(td.path()).await.unwrap();
        assert!(matches!(items[0].kind, MediaKind::Movie));
    }

    #[test]
    fn sxxeyy_token_recognises_common_patterns() {
        assert!(has_sxxeyy_token("Show.S01E02.mkv"));
        assert!(has_sxxeyy_token("show s1e1.mp4"));
        assert!(has_sxxeyy_token("Series_S12E07_HDTV.mkv"));
        assert!(!has_sxxeyy_token("classS5English.mp4")); // mid-word "S5" rejected
        assert!(!has_sxxeyy_token("Movie 2024.mkv"));
    }

    #[test]
    fn parses_series_info_from_canonical_layout() {
        let p = Path::new("/srv/media/TV/My Show/Season 2/My.Show.S02E07.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "My Show");
        assert_eq!(info.season_number, Some(2));
        assert_eq!(info.episode_number, Some(7));
    }

    #[test]
    fn parses_series_info_with_compact_season_dir() {
        let p = Path::new("/m/Another Show/S03/file.s03e01.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Another Show");
        assert_eq!(info.season_number, Some(3));
        assert_eq!(info.episode_number, Some(1));
    }

    #[test]
    fn parses_series_info_specials_is_season_zero() {
        let p = Path::new("/m/Some Show/Specials/Some.Show.S00E04.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Some Show");
        assert_eq!(info.season_number, Some(0));
        assert_eq!(info.episode_number, Some(4));
    }

    #[test]
    fn series_info_falls_back_to_filename_season_when_no_season_dir() {
        let p = Path::new("/m/Show Without Season Dir/Show.S05E11.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Show Without Season Dir");
        assert_eq!(info.season_number, Some(5));
        assert_eq!(info.episode_number, Some(11));
    }

    #[test]
    fn season_dir_patterns_recognised() {
        assert!(looks_like_season_dir("Season 1"));
        assert!(looks_like_season_dir("season 02"));
        assert!(looks_like_season_dir("S01"));
        assert!(looks_like_season_dir("Specials"));
        assert!(!looks_like_season_dir("Movies"));
        assert!(!looks_like_season_dir("Some Movie 2024"));
    }

    #[tokio::test]
    async fn stable_id_is_deterministic() {
        let a = stable_id(Path::new("/srv/media/movie.mkv"));
        let b = stable_id(Path::new("/srv/media/movie.mkv"));
        assert_eq!(a, b);
        let c = stable_id(Path::new("/srv/media/other.mkv"));
        assert_ne!(a, c);
    }

    #[test]
    fn split_edition_tag_recognises_known_editions() {
        // P41 — the matcher requires the trailing ` - <known>` so a
        // movie called "Crouching Tiger - Original" splits, but
        // "Crouching Tiger - Hidden Dragon" does not (Hidden Dragon
        // is not a known edition tag).
        assert_eq!(
            split_edition_tag("Movie Title - Director's Cut"),
            Some(("Movie Title", "Director's Cut"))
        );
        assert_eq!(
            split_edition_tag("The Film - Extended"),
            Some(("The Film", "Extended"))
        );
        assert_eq!(split_edition_tag("Crouching Tiger - Hidden Dragon"), None);
    }

    #[test]
    fn group_editions_pairs_primary_with_director_cut_alternate() {
        // P41 — `Movie.mkv` + `Movie - Director's Cut.mkv` in the same
        // directory becomes one MediaItem with a single
        // AlternateMediaSource hanging off the primary's probe.
        let dir = std::path::PathBuf::from("/srv/m");
        let primary = dir.join("Movie.mkv");
        let alt = dir.join("Movie - Director's Cut.mkv");
        let groups = group_editions(vec![primary.clone(), alt.clone()]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, primary);
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[0].1[0].0, "Director's Cut");
        assert_eq!(groups[0].1[0].1, alt);
    }

    #[test]
    fn group_editions_keeps_orphan_alts_standalone() {
        // P41 — an edition file with no matching primary in the same
        // directory still surfaces as a standalone library item so a
        // user-curated rip doesn't disappear from the catalog.
        let dir = std::path::PathBuf::from("/srv/m");
        let orphan = dir.join("OnlyEdition - Extended.mkv");
        let groups = group_editions(vec![orphan.clone()]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, orphan);
        assert!(groups[0].1.is_empty());
    }

    #[test]
    fn edition_id_slug_is_url_safe() {
        assert_eq!(edition_id_slug("Director's Cut"), "director-s-cut");
        assert_eq!(edition_id_slug("IMAX"), "imax");
        assert_eq!(edition_id_slug("Extended Edition"), "extended-edition");
    }
}
