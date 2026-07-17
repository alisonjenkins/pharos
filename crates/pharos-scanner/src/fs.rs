//! Recursive media filesystem scan. Generic over `Prober` (V12).
//! Walk lives in `spawn_blocking` — never parks async runtime (V5).

use futures_util::stream::StreamExt;
use pharos_core::{
    AlternateMediaSource, ArtworkSource, CollectionStore, DomainError, DomainResult, Fingerprint,
    GenreStore, MediaId, MediaItem, MediaKind, MediaStore, MetadataRequest, MetadataResult,
    PersonStore, Prober, ScanOutcome, Scanner, SeriesInfo, StudioStore, TagStore,
};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

use crate::metadata::{
    embedded::EmbeddedTagProvider, filename::FilenameProvider, nfo::NfoProvider,
    sidecar::SidecarArtworkProvider, MetadataResolver,
};

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

/// LIB-A5 — hard ceiling on the parallel-probe fan-out a caller may request
/// via [`FsScanner::with_probe_concurrency`]. Above this, disk seek thrash and
/// (on shared storage) link saturation cost more than they buy.
const MAX_PROBE_CONCURRENCY: usize = 8;

/// LIB-A5 / #11 — DEFAULT probe fan-out. Deliberately below the CPU count: the
/// practical bottleneck for a library scan is usually shared-storage I/O
/// (NFS/SMB), not CPU, so a high fan-out saturates the link and starves
/// foreground reads — subtitle extraction, HLS segments, trickplay generation —
/// even on a many-core box (observed live: 8 concurrent remux probes over NFS
/// pushed a 15 s subtitle extract past the 60 s request timeout). Cap the
/// default low to leave I/O headroom; local-storage deployments that want full
/// speed set `[server].scan_probe_concurrency` (or call
/// `with_probe_concurrency`) explicitly.
const DEFAULT_PROBE_CONCURRENCY: usize = 4;

/// Default probe fan-out: available CPU parallelism, clamped to
/// `[1, DEFAULT_PROBE_CONCURRENCY]`. Falls back to 1 if the platform can't
/// report parallelism.
fn default_probe_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, DEFAULT_PROBE_CONCURRENCY)
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

#[derive(Clone)]
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
    /// LIB-D7 — local-first metadata resolution. Consulted in the parallel
    /// probe phase (its NFO read + sidecar `stat` are FS IO, off the async
    /// reactor — V5) and the merged [`MetadataResult`] is written alongside
    /// the item in the serial write phase. `Arc` so the scanner stays cheap
    /// to `Clone` (the resolver's boxed providers aren't `Clone`). Defaults
    /// to the local provider set (NFO ▸ sidecar ▸ filename); swappable via
    /// [`with_resolver`](Self::with_resolver) for tests.
    resolver: Arc<MetadataResolver>,
    /// When set, the incremental `(mtime, size)` skip is bypassed and every
    /// file is re-probed regardless of its stored signature. The recovery
    /// path for a probe-schema change (e.g. newly extracted MediaAttachments)
    /// that leaves on-disk files byte-identical, so the incremental scan would
    /// never otherwise re-read them. Off by default (`pharos scan --force`).
    force: bool,
    /// Adaptive I/O backpressure gate. When set, every per-file probe (the
    /// heavy NFS read — fingerprint + demux) must first acquire a permit from
    /// this shared semaphore before running. The server shrinks the semaphore's
    /// available permits while live playback is active (see
    /// `AppState::spawn_bg_io_regulator`), so a background re-scan paces itself
    /// down to a trickle during streaming instead of saturating shared storage,
    /// yet keeps making progress. `None` (CLI scans, tests) = full throttle,
    /// bounded only by `probe_concurrency`.
    io_gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl<P: Prober> std::fmt::Debug for FsScanner<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsScanner")
            .field("extensions", &self.extensions.len())
            .field("rate_limit", &self.rate_limit)
            .field("probe_concurrency", &self.probe_concurrency)
            .field("metadata_providers", &self.resolver.provider_count())
            .finish_non_exhaustive()
    }
}

/// LIB-D7 — the default local-first provider set wired into every
/// production scanner: the Kodi NFO reader (highest priority — a
/// user-curated `.nfo` wins), sidecar artwork detection, embedded
/// container-tag descriptions (B90), and filename / folder conventions
/// (lowest — a last-resort title/year guess). Online providers slot in
/// later at lower priority than NFO.
fn default_resolver() -> MetadataResolver {
    MetadataResolver::new()
        .with_provider(NfoProvider::new())
        .with_provider(SidecarArtworkProvider::new())
        .with_provider(EmbeddedTagProvider::new())
        .with_provider(FilenameProvider::new())
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
            resolver: Arc::new(default_resolver()),
            force: false,
            io_gate: None,
        }
    }

    pub fn with_extensions(prober: P, exts: impl IntoIterator<Item = String>) -> Self {
        Self {
            prober,
            extensions: exts.into_iter().collect(),
            rate_limit: std::time::Duration::ZERO,
            probe_concurrency: default_probe_concurrency(),
            resolver: Arc::new(default_resolver()),
            force: false,
            io_gate: None,
        }
    }

    /// Force a full re-probe: bypass the incremental `(mtime, size)` skip so
    /// every file is re-read even when unchanged on disk. Used by
    /// `pharos scan --force` to backfill fields added by a probe-schema change
    /// (e.g. embedded-font MediaAttachments) onto already-indexed items.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Attach an adaptive I/O backpressure gate shared with the live server.
    /// Every heavy per-file probe acquires a permit from `gate` before reading,
    /// so the server can throttle a background re-scan down while playback is
    /// active (by parking most of the gate's permits) without pausing the scan
    /// outright. See [`io_gate`](Self::io_gate). CLI scans leave it unset.
    pub fn with_io_gate(mut self, gate: Arc<tokio::sync::Semaphore>) -> Self {
        self.io_gate = Some(gate);
        self
    }

    /// LIB-D7 — override the metadata resolver. Production callers use the
    /// default local provider set (NFO ▸ sidecar ▸ filename); this is a test
    /// seam to inject a fixed/empty resolver, mirroring
    /// [`with_probe_concurrency`](Self::with_probe_concurrency).
    pub fn with_resolver(mut self, resolver: MetadataResolver) -> Self {
        self.resolver = Arc::new(resolver);
        self
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

    /// #11 — apply a config-supplied probe fan-out. `0` keeps the conservative
    /// [`default_probe_concurrency`] (leaves shared-storage I/O headroom);
    /// non-zero overrides it, clamped to `[1, MAX_PROBE_CONCURRENCY]`.
    pub fn with_probe_concurrency_opt(self, degree: usize) -> Self {
        if degree == 0 {
            self
        } else {
            self.with_probe_concurrency(degree)
        }
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
    pub async fn scan_into<
        S: MediaStore + GenreStore + PersonStore + StudioStore + CollectionStore + TagStore,
    >(
        &self,
        root: &Path,
        store: &S,
    ) -> DomainResult<ScanOutcome> {
        let scan_id = store.begin_scan(root).await?;
        let WalkOutcome {
            files: paths,
            errors: walk_errors,
        } = walk(root.to_path_buf(), self.extensions.clone()).await?;
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
        // Buffer the skip-path `mark_seen` stamps (the bulk of scan writes — one
        // per unchanged file) and flush them in transactions, so N unchanged
        // files cost a handful of WAL commits instead of N autocommit fsyncs
        // (the occasional ~1s `UPDATE media_items` stalls seen under scan load).
        // Chunked so the write lock is never held for a whole huge library.
        const SEEN_FLUSH: usize = 512;
        let mut seen_batch: Vec<(pharos_core::MediaId, i64, u64)> = Vec::new();
        for (primary, alts) in groups {
            if seen_batch.len() >= SEEN_FLUSH {
                store.mark_seen_batch(&seen_batch, scan_id).await?;
                seen_batch.clear();
            }
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
                // signature still matches; otherwise re-probe + put. `--force`
                // (self.force) bypasses the skip so every file is re-probed.
                if let Some((mtime, size)) = sig {
                    if !self.force
                        && state.file_mtime == mtime
                        && state.file_size == size
                        && state.probe_schema_version == pharos_core::PROBE_SCHEMA_VERSION
                    {
                        seen_batch.push((id, mtime, size));
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
                        seen_batch.push((c.id, mtime, size));
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
                        seen_batch.push((c.id, mtime, size));
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
        // Flush the remaining skip-path stamps before the probe stream + sweep
        // (sweep deletes rows not stamped with this scan_id, so every seen file
        // must be committed first).
        store.mark_seen_batch(&seen_batch, scan_id).await?;
        seen_batch.clear();

        // Phase 2 + 3 — bounded-concurrency probe stream feeding a sequential
        // write consumer. `buffer_unordered` keeps at most `probe_concurrency`
        // probes in flight; results are awaited (in completion order) and the
        // store writes applied one at a time on this task. A probe that
        // returns `None` (failure / unrecognised) is logged inside
        // `probe_with_alternates`/`probe_one` and simply produces no write
        // (V6 — one bad file never aborts the batch).
        let rate_limit = self.rate_limit;
        let io_gate = self.io_gate.clone();
        let mut stream = futures_util::stream::iter(pending)
            .map(|p| {
                let rl = rate_limit;
                let gate = io_gate.clone();
                async move {
                    // P43 — preserve the inter-probe throttle. Under
                    // parallelism this paces each probe task's start rather
                    // than serialising the whole scan; at degree 1 it matches
                    // the original sequential pause.
                    if !rl.is_zero() {
                        tokio::time::sleep(rl).await;
                    }
                    // Adaptive backpressure — hold a shared I/O permit across
                    // this file's fingerprint + probe (the heavy NFS reads).
                    // While live playback runs the server parks most of the
                    // gate's permits, throttling the whole probe fan-out down
                    // to a trickle so streaming keeps its storage bandwidth;
                    // the permit drops when this file's reads finish. Unset
                    // (CLI/tests) = no gate, full throttle.
                    let _io_permit = match &gate {
                        Some(sem) => sem.clone().acquire_owned().await.ok(),
                        None => None,
                    };
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
                    // LIB-D7 — resolve local-first metadata in the SAME
                    // off-reactor/parallel phase as the probe (its NFO read +
                    // sidecar stat are FS IO — V5). A resolution failure on
                    // one file never aborts the scan (V6); `resolve` returns
                    // an empty result for a missing/partial source and logs +
                    // skips a provider that errors. Only resolve for an item
                    // we actually probed — a probe miss writes nothing.
                    let meta = match &item {
                        Some(it) => self.resolve_metadata(it).await,
                        None => MetadataResult::default(),
                    };
                    (p.id, p.sig, p.existed, fp, item, meta)
                }
            })
            .buffer_unordered(self.probe_concurrency);

        while let Some((_id, sig, existed, fp, item, meta)) = stream.next().await {
            let Some(mut item) = item else { continue };
            let item_id = item.id;
            // LIB-D7 — merge resolved metadata onto the probe-built item
            // BEFORE `put` (which consumes it). `merge_metadata_into_item`
            // also returns the UNION of probe + NFO genres and consumes the
            // artwork refs, so we capture them here (genres + artwork are
            // applied after the row exists — FK on item id).
            let MergedEntities {
                genres,
                artwork,
                people,
                studios,
                collections,
                tags,
            } = merge_metadata_into_item(&mut item, meta);
            store.put(item).await?;
            // LIB-C4 — populate the item_genres join. UNION of the probe's
            // (possibly comma/pipe-separated) `genre` column with the NFO
            // `<genre>` tags resolved above; `link_item_genres` is idempotent
            // (de-dupes), so the union never double-inserts.
            store.link_item_genres(item_id, &genres).await?;
            // LIB-C2 — populate the item_people join from the NFO cast/crew
            // the resolver parsed. `link_item_people` upserts each person row
            // (carrying its headshot / provider ids) then replaces the item's
            // credits wholesale, so a rescan keeps the join current.
            store.link_item_people(item_id, &people).await?;
            // LIB-C3 — populate the item_studios join from the NFO <studio>
            // tags the resolver parsed. `link_item_studios` upserts each
            // studio row then replaces the item's studios wholesale, so a
            // rescan keeps the join current.
            store.link_item_studios(item_id, &studios).await?;
            // LIB-C5 — populate the collection_items membership join from the
            // NFO <set>/<collection> tags the resolver parsed. Each named box
            // set is upserted and the item appended (idempotent), so a rescan
            // keeps NFO-driven box-set membership current alongside any manual
            // CRUD additions.
            store.link_item_collections(item_id, &collections).await?;
            // LIB-C6 — populate the item_tags join from the NFO <tag>
            // elements + the filename provider's quality/source tokens the
            // resolver parsed. `link_item_tags` upserts each tag row then
            // replaces the item's tags wholesale, so a rescan keeps the join
            // current (a dropped <tag> clears its stale link).
            store.link_item_tags(item_id, &tags).await?;
            // LIB-D7 — persist discovered artwork (local sidecars from D4 +
            // any NFO thumb/fanart URLs) keyed by item id + role. One row per
            // role; the resolver fed refs in priority order so the first per
            // role is the winner. `set_artwork` upserts.
            persist_artwork(store, item_id, &artwork).await?;
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
        //
        // BUT only sweep when the walk was COMPLETE. If any entry was
        // unreadable this pass (an *arr import mid-move, a momentary NFS
        // EPERM, a transiently-unlistable subdir) the listing is a subset of
        // what's really on disk — a still-present file could be missing from
        // it. Sweeping then would delete its row and make it vanish from the
        // library until the next scan re-adds it (the "disappearing media"
        // symptom). Skip the sweep this pass; a later clean scan reconciles
        // genuine deletions. (Deletion is merely delayed, never wrong.)
        let root_prefix = root.to_string_lossy();
        let swept = if walk_errors == 0 {
            store.sweep_unseen(scan_id, &root_prefix).await?
        } else {
            tracing::warn!(
                scan_id,
                walk_errors,
                "walk incomplete ({walk_errors} unreadable entries); skipping deletion sweep this pass to avoid pruning transiently-unreadable files",
            );
            Vec::new()
        };
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
    pub async fn update_path<
        S: MediaStore + GenreStore + PersonStore + StudioStore + CollectionStore + TagStore,
    >(
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
                if !self.force && state.file_mtime == mtime && state.file_size == size {
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
    #[tracing::instrument(skip(self, store, sig, fp), fields(media.path = %path.display()))]
    async fn probe_put_one<
        S: MediaStore + GenreStore + PersonStore + StudioStore + CollectionStore + TagStore,
    >(
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
        let Some(mut item) = self.probe_one(path.to_path_buf()).await else {
            // V6 — probe failed, already logged in `probe_one`. No write.
            return Ok(PathUpdate::Skipped);
        };
        let item_id = item.id;
        // LIB-D7 — resolve local-first metadata for the (re)probed file, off
        // the reactor, and merge it onto the item before `put` (mirrors the
        // batch scan_into path so a watched create/modify enriches the same
        // way a full scan does). A bad NFO / sidecar is logged + skipped by
        // the resolver (V6); the item still imports from probe data.
        let meta = self.resolve_metadata(&item).await;
        let MergedEntities {
            genres,
            artwork,
            people,
            studios,
            collections,
            tags,
        } = merge_metadata_into_item(&mut item, meta);
        store.put(item).await?;
        // LIB-C4 — keep the item_genres join in step (probe ∪ NFO genres).
        store.link_item_genres(item_id, &genres).await?;
        // LIB-C2 — keep the item_people join in step (watched create/modify
        // enriches cast/crew the same way a full scan does).
        store.link_item_people(item_id, &people).await?;
        // LIB-C3 — keep the item_studios join in step (watched create/modify
        // enriches studios the same way a full scan does).
        store.link_item_studios(item_id, &studios).await?;
        // LIB-C5 — keep the collection_items join in step (watched
        // create/modify links NFO <set> box-set membership the same way a
        // full scan does).
        store.link_item_collections(item_id, &collections).await?;
        // LIB-C6 — keep the item_tags join in step (watched create/modify
        // links NFO <tag> + filename quality/source tokens the same way a
        // full scan does).
        store.link_item_tags(item_id, &tags).await?;
        // LIB-D7 — persist discovered artwork (FK on the row just put).
        persist_artwork(store, item_id, &artwork).await?;
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

    /// LIB-D7 — resolve local-first metadata for a freshly-probed `item`.
    /// Builds a [`MetadataRequest`] borrowing the item's path / kind / probe
    /// / series and runs the resolver (NFO ▸ sidecar ▸ filename). Runs in the
    /// parallel probe phase so the NFO read + sidecar `stat` stay off the
    /// async reactor (V5). The resolver never returns `Err` — a malformed
    /// NFO / unreadable sidecar is logged + skipped inside `resolve` (V6) and
    /// the item is still imported from probe data.
    async fn resolve_metadata(&self, item: &MediaItem) -> MetadataResult {
        let req = MetadataRequest {
            path: &item.path,
            kind: item.kind,
            probe: &item.probe,
            series: item.series.as_ref(),
        };
        self.resolver.resolve(&req).await
    }

    #[tracing::instrument(skip(self), fields(media.path = %path.display()))]
    async fn probe_one(&self, path: PathBuf) -> Option<MediaItem> {
        match self.prober.probe(&path).await {
            Ok(info) => {
                // Audio: the embedded track title (ID3/Vorbis TITLE) is the
                // authoritative song name — a filename stem is often just a
                // track number ("02 Stars") or the album name. Fall back to
                // the stem when the file carries no title tag. Video keeps
                // the stem (the NFO/clean resolver refines it later).
                let stem = || {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string()
                };
                let title = match info.kind {
                    MediaKind::Audio => info
                        .probe
                        .title
                        .clone()
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(stem),
                    _ => stem(),
                };
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

/// LIB-D7 — the entity side-effects [`merge_metadata_into_item`] hands
/// back for the caller to link once the media row exists (the joins carry
/// FKs on the item id, so they must follow `store.put`). Each `Vec` is the
/// resolved-and-deduped set for one entity kind.
struct MergedEntities {
    /// Genre names — UNION of the probe `genre` column + NFO `<genre>`.
    genres: Vec<String>,
    /// Resolved artwork refs (local sidecars + NFO thumb/fanart URLs).
    artwork: Vec<pharos_core::ArtworkRef>,
    /// Cast/crew credits (LIB-C2 `item_people`).
    people: Vec<pharos_core::PersonRef>,
    /// Studio names (LIB-C3 `item_studios`).
    studios: Vec<String>,
    /// Collection / box-set names (LIB-C5 `collection_items`).
    collections: Vec<String>,
    /// Free-form tag names (LIB-C6 `item_tags`) — NFO `<tag>` + the
    /// filename provider's quality/source tokens.
    tags: Vec<String>,
}

/// LIB-D7 — merge a resolved [`MetadataResult`] onto a probe-built
/// [`MediaItem`] in place, then return the [`MergedEntities`] the caller
/// must link once the row exists.
///
/// Scalars (`overview` / `tagline` / `production_year` / `premiere_date` /
/// ratings / `official_rating` / each `provider_ids` slot) overwrite the
/// item's `Default`-empty [`MediaMetadata`] when the resolver found a value
/// (the resolver already applied priority — first `Some` wins — so anything
/// present here is the winning source). The **title** prefers the
/// NFO/clean resolver title over the raw filename stem the probe set.
///
/// Genres are the UNION of the probe's `genre` column (split on `|`/`,`) and
/// the NFO `<genre>` tags, de-duped in that order (probe first for stable
/// ordering). `people` (LIB-C2) / `studios` (LIB-C3) / `collections`
/// (LIB-C5) / `tags` (LIB-C6) are all returned in [`MergedEntities`] so the
/// caller links them into their respective joins after `put` — nothing from
/// the [`MetadataResult`] is dropped any more.
fn merge_metadata_into_item(item: &mut MediaItem, meta: MetadataResult) -> MergedEntities {
    let MetadataResult {
        title,
        overview,
        tagline,
        production_year,
        premiere_date,
        community_rating,
        critic_rating,
        official_rating,
        genres: nfo_genres,
        studios,
        people,
        tags,
        collections,
        production_locations,
        trailers,
        provider_ids,
        artwork,
    } = meta;

    // Title: prefer a non-empty resolved (NFO/clean) title over the raw
    // filename stem the probe assigned. A blank NFO <title> is ignored.
    if let Some(t) = title {
        let t = t.trim();
        if !t.is_empty() {
            item.title = t.to_string();
        }
    }

    let md = &mut item.metadata;
    set_some(&mut md.overview, overview);
    set_some(&mut md.tagline, tagline);
    set_some(&mut md.production_year, production_year);
    set_some(&mut md.premiere_date, premiere_date);
    set_some(&mut md.community_rating, community_rating);
    set_some(&mut md.critic_rating, critic_rating);
    set_some(&mut md.official_rating, official_rating);
    set_some(&mut md.provider_ids.tmdb, provider_ids.tmdb);
    set_some(&mut md.provider_ids.tvdb, provider_ids.tvdb);
    set_some(&mut md.provider_ids.imdb, provider_ids.imdb);
    set_some(&mut md.provider_ids.mbid, provider_ids.mbid);
    // T67 — production countries (`ProductionLocations`) + trailer URLs
    // (`RemoteTrailers`) live directly on the metadata (Vec<String>), not a
    // join. The resolver already merged/deduped across NFO layers; normalise
    // (trim + drop blanks) once more so a stray blank never reaches the wire.
    md.production_locations = normalise_list(production_locations);
    md.trailers = normalise_list(trailers);

    // Genre union: probe column first (stable order), then NFO tags.
    let probe_genres = item
        .probe
        .genre
        .as_deref()
        .map(pharos_core::split_genre_field)
        .unwrap_or_default();
    let mut genres: Vec<String> = Vec::with_capacity(probe_genres.len() + nfo_genres.len());
    for g in probe_genres.into_iter().chain(nfo_genres) {
        let g = g.trim().to_string();
        if !g.is_empty() && !genres.iter().any(|e| e == &g) {
            genres.push(g);
        }
    }

    // LIB-C6 — tags (NFO `<tag>` + filename quality/source tokens) are now
    // a real join: normalise (trim + drop blanks + de-dup, preserving the
    // resolver's source order) and hand them to the caller to link after
    // `put`. The last entity that was logged-and-dropped now persists.
    let mut merged_tags: Vec<String> = Vec::with_capacity(tags.len());
    for t in tags {
        let t = t.trim().to_string();
        if !t.is_empty() && !merged_tags.iter().any(|e| e == &t) {
            merged_tags.push(t);
        }
    }

    MergedEntities {
        genres,
        artwork,
        people,
        studios,
        collections,
        tags: merged_tags,
    }
}

/// Overwrite `slot` with `value` only when the resolver produced one. The
/// item's metadata starts `Default`-empty, so this is just "take the
/// resolved value when present" — priority was already applied by the
/// resolver's first-`Some`-wins merge.
fn set_some<T>(slot: &mut Option<T>, value: Option<T>) {
    if value.is_some() {
        *slot = value;
    }
}

/// Trim, drop blanks, and de-dup (preserving order) a resolved string list.
fn normalise_list(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for s in items {
        let s = s.trim().to_string();
        if !s.is_empty() && !out.iter().any(|e| e == &s) {
            out.push(s);
        }
    }
    out
}

/// LIB-D7 — persist resolved artwork refs via [`MediaStore::set_artwork`],
/// one upsert per `(item, role)`. A [`ArtworkSource::LocalFile`] is recorded
/// as `source="local"` with the absolute path; a [`ArtworkSource::Url`] as
/// `source="url"` for a later download pass. The resolver fed refs in
/// priority order and de-duped on `(role, source)`; the first per role wins,
/// and `set_artwork`'s upsert keeps it.
async fn persist_artwork<S: MediaStore>(
    store: &S,
    item_id: MediaId,
    artwork: &[pharos_core::ArtworkRef],
) -> DomainResult<()> {
    let mut written: HashSet<&'static str> = HashSet::new();
    for art in artwork {
        let role = art.role.as_str();
        // One row per role; the first ref for a role (highest priority,
        // post-dedupe) is the winner — skip later refs for the same role so
        // a lower-priority source can't clobber the upsert.
        if !written.insert(role) {
            continue;
        }
        let (source, locator) = match &art.source {
            ArtworkSource::LocalFile(p) => ("local", p.to_string_lossy().into_owned()),
            ArtworkSource::Url(u) => ("url", u.clone()),
        };
        store.set_artwork(item_id, role, source, &locator).await?;
    }
    Ok(())
}

impl<P: Prober + Clone + 'static> Scanner for FsScanner<P> {
    #[tracing::instrument(skip(self), fields(root = %root.display()))]
    async fn scan(&self, root: &Path) -> DomainResult<Vec<MediaItem>> {
        // The bare `scan` (no incremental sweep) only needs the file list; a
        // partial walk (unreadable entries logged inside `walk`) still yields
        // every readable file.
        let paths = walk(root.to_path_buf(), self.extensions.clone())
            .await?
            .files;
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
pub(crate) const KNOWN_EDITIONS: &[&str] = &[
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

pub(crate) fn is_known_edition(s: &str) -> bool {
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
    // Full Emby.Naming-style extraction: SxxEyy, 1x02, EP12, "Episode 12",
    // and absolute anime numbering ("Series - 07") all resolve here.
    let parsed = parse_episode_from_name(name);
    let filename_season = parsed.and_then(|(s, _)| s);
    let episode = parsed.map(|(_, e)| e);

    // Walk parents from closest to farthest, retaining the &Path so the
    // first non-season ancestor's FULL path becomes the C11 folder key.
    let parents: Vec<&Path> = path.ancestors().skip(1).collect();

    let mut season_from_dir: Option<u32> = None;
    let mut series_name: Option<String> = None;
    let mut series_folder: Option<PathBuf> = None;

    for parent in parents {
        let Some(component) = parent.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(n) = parse_season_dir(component) {
            season_from_dir = season_from_dir.or(Some(n));
            continue;
        }
        if component.eq_ignore_ascii_case("specials") {
            season_from_dir = season_from_dir.or(Some(0));
            continue;
        }
        // First non-season ancestor wins as the series. LIB-C11: capture
        // both the display name AND the canonical folder path (the show
        // root that holds the season dirs / episodes) for identity.
        series_name = Some(component.to_string());
        series_folder = Some(parent.to_path_buf());
        break;
    }

    let mut series_name = series_name?;
    let mut season_number = season_from_dir.or(filename_season);
    // An absolute-numbered episode with no season declared belongs to season 1
    // (Jellyfin's convention) so it groups + orders cleanly rather than under a
    // null season — without this every such episode sorts equal and renders in
    // scan order.
    if season_number.is_none() && episode.is_some() {
        season_number = Some(1);
    }
    // LIB-C11: parse a `Show Name (YYYY)` year from the folder component.
    let series_year = series_folder
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .and_then(parse_folder_year);
    // When a year was parsed, strip the trailing `(YYYY)` from the display
    // name so the title stays clean ("Cosmos", not "Cosmos (1980)") while
    // identity stays keyed on the full folder path.
    if series_year.is_some() {
        series_name = strip_trailing_year(&series_name).to_string();
    }
    let series_folder = series_folder.map(|p| p.to_string_lossy().into_owned());
    Some(SeriesInfo {
        series_name,
        season_number,
        episode_number: episode,
        series_folder,
        series_year,
    })
}

/// LIB-C11 — drop a trailing `(YYYY)` marker from a show display name so
/// the clean title surfaces while identity stays keyed on the folder
/// path. Only strips when [`parse_folder_year`] would match, so a name
/// without a year marker is returned untouched.
fn strip_trailing_year(name: &str) -> &str {
    if parse_folder_year(name).is_none() {
        return name;
    }
    let trimmed = name.trim_end();
    match trimmed.rfind('(') {
        Some(open) => trimmed[..open].trim_end(),
        None => name,
    }
}

/// LIB-C11 — extract the release year from a `Show Name (YYYY)` folder
/// convention. Matches a 4-digit year in trailing parentheses
/// (`Cosmos (1980)` → `1980`). Returns `None` when the folder carries no
/// such marker. Restricted to a plausible 1800–2999 window so a
/// parenthesised non-year (e.g. `(Uncut)`, `(1)`) doesn't masquerade.
pub(crate) fn parse_folder_year(folder_name: &str) -> Option<u32> {
    let trimmed = folder_name.trim_end();
    let close = trimmed.strip_suffix(')')?;
    let open = close.rfind('(')?;
    let inner = close[open + 1..].trim();
    if inner.len() != 4 || !inner.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: u32 = inner.parse().ok()?;
    (1800..3000).contains(&year).then_some(year)
}

/// One ordered episode-number expression, ported from Emby.Naming's
/// `EpisodeExpressions`. `optimistic` mirrors Jellyfin's `IsOptimistic`
/// flag: optimistic expressions run in a SECOND pass, only after the
/// stricter first pass found nothing (so a title that merely contains a
/// number doesn't outrank a real `SxxEyy`).
struct EpExpr {
    re: regex::Regex,
    optimistic: bool,
}

/// The ported expression set. Faithful to Jellyfin's ordering + two-pass
/// (non-optimistic then optimistic) semantics, but with two deliberate
/// simplifications: pharos derives the series NAME from the folder tree, so
/// the `seriesname` captures (and the negative-lookaround guards that only
/// exist to bound them — Rust's `regex` has no lookaround) are dropped; and
/// each pattern ends right after the episode-number group so the
/// `read_episode_number` position guard (Jellyfin's `0-9iIpP` next-char
/// check) can reject resolutions like `1080p`.
///
/// Optimistic absolute-number patterns cap at 3 digits (Jellyfin's `{1,3}`)
/// so a 4-digit release year can't masquerade as an episode.
static EP_EXPRS: std::sync::LazyLock<Vec<EpExpr>> = std::sync::LazyLock::new(|| {
    // Panic-free construction (workspace denies unwrap/expect): a pattern
    // that fails to compile is logged + skipped rather than aborting. All
    // patterns are constants covered by unit tests, so this never drops one
    // in practice.
    fn compile(pattern: &str, optimistic: bool) -> Option<EpExpr> {
        match regex::Regex::new(pattern) {
            Ok(re) => Some(EpExpr { re, optimistic }),
            Err(e) => {
                tracing::error!(pattern, error = %e, "invalid episode regex — skipped");
                None
            }
        }
    }
    [
        // ---- first pass: explicit season+episode / episode markers ----
        // SxxExx, S01 E02, S01.E02, S01xE02, S01E02-E03 (start captured).
        compile(r"(?i)s(?<s>\d{1,4})[\]\[ ._x-]*e(?<e>\d+)", false),
        // Season 1 Episode 2 (words, any separators).
        compile(
            r"(?i)season[ ._-]*(?<s>\d{1,4})[ ._-]*episode[ ._-]*(?<e>\d+)",
            false,
        ),
        // 1x02, 01x02, S01x02 (the CxE / NxNN convention).
        compile(r"(?i)(?:^|[\\/._ \[(-])s?(?<s>\d{1,4})x(?<e>\d+)", false),
        // EP12 / EP_12.
        compile(r"(?i)[\\/._ \[(-]ep_?(?<e>\d+)", false),
        // Episode 12 (word, no season).
        compile(r"(?i)episode[ ._-]*(?<e>\d+)", false),
        // E12 at a separator boundary (Show E01, name.E05.).
        compile(r"(?i)[\\/._ \[(-]e(?<e>\d+)", false),
        // ---- second pass: optimistic absolute numbering (1-3 digits) ----
        // Fansub dash: "Series - 07" (greedy prefix → the LAST " - ").
        compile(r"(?i)^.* - (?<e>\d{1,3})", true),
        compile(r"(?i)^.*[._]-[._](?<e>\d{1,3})", true),
        // Bracketed absolute: "Show [12]".
        compile(r"(?i)\[(?<e>\d{1,3})\]", true),
        // Whole filename is the number: "07.mkv", "07-08.mkv".
        compile(r"(?i)(?:^|[\\/])(?<e>\d{1,3})(?:-\d{2,3})?\.[^\\/]+$", true),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// Parse `(season, episode)` from a filename using the ported Jellyfin
/// expression set. `season` is `None` when the matching expression carries
/// no season (absolute / episode-only forms). Returns `None` when no
/// expression yields an episode number.
///
/// Two passes (non-optimistic then optimistic), first match wins — Jellyfin's
/// `EpisodePathParser.Parse` order. Each candidate is validated: the char
/// after the episode digits must not continue a number or a resolution
/// (`0-9 i I p P`), and a season in Jellyfin's junk band (200–1927 or >2500,
/// where resolutions/years land) is rejected.
fn parse_episode_from_name(name: &str) -> Option<(Option<u32>, u32)> {
    for optimistic in [false, true] {
        for expr in EP_EXPRS.iter().filter(|x| x.optimistic == optimistic) {
            let Some(caps) = expr.re.captures(name) else {
                continue;
            };
            let Some(e_match) = caps.name("e") else {
                continue;
            };
            // Resolution / continued-number guard (Emby.Naming's next-char
            // check): the byte after the episode digits must not be another
            // digit or a resolution marker.
            if let Some(&next) = name.as_bytes().get(e_match.end()) {
                if next.is_ascii_digit() || matches!(next, b'i' | b'I' | b'p' | b'P') {
                    continue;
                }
            }
            let Ok(episode) = e_match.as_str().parse::<u32>() else {
                continue;
            };
            let season = caps
                .name("s")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .filter(|s| !(200..=1927).contains(s) && *s <= 2500);
            return Some((season, episode));
        }
    }
    None
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

/// Result of a filesystem [`walk`]: the recognised media file paths, plus a
/// count of entries walkdir could not read this pass. A non-zero `errors`
/// means the listing is *incomplete* — the caller must not treat a missing
/// path as a deletion (see the mark-and-sweep gate in `scan_into`).
struct WalkOutcome {
    files: Vec<PathBuf>,
    errors: usize,
}

/// Recursive walk inside `spawn_blocking`. Returns paths of files whose
/// lowercased extension is in `exts`, plus how many entries were unreadable.
async fn walk(root: PathBuf, exts: HashSet<String>) -> DomainResult<WalkOutcome> {
    tokio::task::spawn_blocking(move || -> DomainResult<WalkOutcome> {
        let mut out = Vec::new();
        let mut errors = 0usize;
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            // V6 — a per-entry walk error (a file an *arr import is moving
            // mid-scan → EPERM/ENOENT on one path, or a subtree we lack
            // permission to descend into) must never abort the whole library
            // scan. walkdir keeps iterating after an errored entry, so log it
            // and skip just that one; every readable file still gets indexed.
            // NFS readdir returns DT_UNKNOWN, so walkdir stats each entry —
            // a file being replaced mid-scan surfaces here, not just at probe.
            let e = match entry {
                Ok(e) => e,
                Err(err) => {
                    errors += 1;
                    tracing::warn!(
                        path = ?err.path(),
                        error = %err,
                        "walk: skipping unreadable entry",
                    );
                    continue;
                }
            };
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
        Ok(WalkOutcome { files: out, errors })
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
        // LIB-C4 — item_genres mirror: item id → its linked genre names.
        item_genres: Mutex<HashMap<MediaId, Vec<String>>>,
        // LIB-D4 — artwork mirror: (item id, role) → (source, locator).
        artwork: Mutex<HashMap<(MediaId, String), (String, String)>>,
        // LIB-C2 — item_people mirror: item id → its linked credits.
        item_people: Mutex<HashMap<MediaId, Vec<pharos_core::PersonRef>>>,
        // LIB-C3 — item_studios mirror: item id → its linked studio names.
        item_studios: Mutex<HashMap<MediaId, Vec<String>>>,
        // LIB-C5 — collections mirror: collection name → ordered member ids
        // (the curated sort_order). Append-on-link, idempotent.
        collections: Mutex<HashMap<String, Vec<MediaId>>>,
        // LIB-C6 — item_tags mirror: item id → its linked tag names.
        item_tags: Mutex<HashMap<MediaId, Vec<String>>>,
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
        async fn query(&self, q: &pharos_core::MediaQuery) -> DomainResult<(Vec<MediaItem>, u64)> {
            // Scanner test store: kind filter + id-ordered page + total.
            // Entity / parent pivots live in the SQL backends, not here.
            let mut items: Vec<MediaItem> = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .values()
                .filter(|i| q.kinds.is_empty() || q.kinds.contains(&i.kind))
                .cloned()
                .collect();
            items.sort_by_key(|i| i.id);
            let total = items.len() as u64;
            let start = usize::try_from(q.start_index).unwrap_or(usize::MAX);
            let mut page: Vec<MediaItem> = items.into_iter().skip(start).collect();
            if let Some(limit) = q.limit {
                page.truncate(limit as usize);
            }
            Ok((page, total))
        }
        async fn search(
            &self,
            q: &pharos_core::SearchQuery,
        ) -> DomainResult<(Vec<MediaItem>, u64)> {
            // Scanner test store: title/overview substring superset, kind
            // filter, id-ordered. The SQL backends carry the real FTS.
            let tokens = pharos_core::search_tokens(&q.term);
            if tokens.is_empty() {
                return Ok((Vec::new(), 0));
            }
            let needle = q.term.trim().to_lowercase();
            let mut items: Vec<MediaItem> = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .values()
                .filter(|i| q.kinds.is_empty() || q.kinds.contains(&i.kind))
                .filter(|i| {
                    i.title.to_lowercase().contains(&needle)
                        || i.metadata
                            .overview
                            .as_deref()
                            .map(|o| o.to_lowercase().contains(&needle))
                            .unwrap_or(false)
                })
                .cloned()
                .collect();
            items.sort_by_key(|i| i.id);
            let total = items.len() as u64;
            let start = usize::try_from(q.offset).unwrap_or(usize::MAX);
            let mut page: Vec<MediaItem> = items.into_iter().skip(start).collect();
            page.truncate(q.limit.max(1) as usize);
            Ok((page, total))
        }
        async fn facets(
            &self,
            _base: &pharos_core::MediaQuery,
            _req: &pharos_core::FacetRequest,
        ) -> DomainResult<pharos_core::MediaFacets> {
            // Scanner test store has no entity tables; facets aren't
            // exercised here.
            Ok(pharos_core::MediaFacets::default())
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
                        // Mirror the store: mark_seen stamps the current version.
                        probe_schema_version: pharos_core::PROBE_SCHEMA_VERSION,
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

        async fn set_artwork(
            &self,
            item_id: MediaId,
            role: &str,
            source: &str,
            locator: &str,
        ) -> DomainResult<()> {
            self.artwork
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(
                    (item_id, role.to_string()),
                    (source.to_string(), locator.to_string()),
                );
            Ok(())
        }

        async fn artwork_for(
            &self,
            item_id: MediaId,
        ) -> DomainResult<Vec<(String, String, String)>> {
            let map = self
                .artwork
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut out: Vec<(String, String, String)> = map
                .iter()
                .filter(|((iid, _), _)| *iid == item_id)
                .map(|((_, role), (source, locator))| {
                    (role.clone(), source.clone(), locator.clone())
                })
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(out)
        }
    }

    // LIB-C4 — minimal in-memory GenreStore so the scanner's
    // link-on-write path can be exercised without the sqlx store.
    impl GenreStore for MemStore {
        async fn upsert_genre(&self, name: &str) -> DomainResult<i64> {
            // Deterministic surrogate id from the wire id's prefix.
            let wid = pharos_core::genre_wire_id(name);
            let id = i64::from_str_radix(&wid[..15], 16).unwrap_or(0);
            Ok(id)
        }
        async fn link_item_genres(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
            let mut wanted: Vec<String> = Vec::new();
            for n in names {
                let t = n.trim();
                if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                    wanted.push(t.to_string());
                }
            }
            self.item_genres
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(item, wanted);
            Ok(())
        }
        async fn genres_with_counts(&self) -> DomainResult<Vec<pharos_core::GenreCount>> {
            use std::collections::BTreeMap;
            let links = self
                .item_genres
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut counts: BTreeMap<String, u32> = BTreeMap::new();
            for names in links.values() {
                for n in names {
                    *counts.entry(n.clone()).or_insert(0) += 1;
                }
            }
            Ok(counts
                .into_iter()
                .map(|(name, item_count)| pharos_core::GenreCount {
                    genre: pharos_core::Genre {
                        id: 0,
                        wire_id: pharos_core::genre_wire_id(&name),
                        name,
                    },
                    item_count,
                })
                .collect())
        }
        async fn item_ids_for_genre(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
            let links = self
                .item_genres
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut ids: Vec<MediaId> = links
                .iter()
                .filter(|(_, names)| {
                    names
                        .iter()
                        .any(|n| pharos_core::genre_wire_id(n) == wire_id)
                })
                .map(|(id, _)| *id)
                .collect();
            ids.sort_unstable();
            Ok(ids)
        }
        async fn backfill_genres(&self) -> DomainResult<u64> {
            let items: Vec<(MediaId, Option<String>)> = {
                let inner = self
                    .inner
                    .lock()
                    .map_err(|e| DomainError::Backend(e.to_string()))?;
                inner
                    .iter()
                    .map(|(id, item)| (*id, item.probe.genre.clone()))
                    .collect()
            };
            for (id, genre) in items {
                if let Some(raw) = genre {
                    let names = pharos_core::split_genre_field(&raw);
                    if !names.is_empty() {
                        self.link_item_genres(id, &names).await?;
                    }
                }
            }
            let total: usize = self
                .item_genres
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .values()
                .map(Vec::len)
                .sum();
            Ok(total as u64)
        }
    }

    // LIB-C2 — minimal in-memory PersonStore so the scanner's
    // link-on-write path can be exercised without the sqlx store.
    impl PersonStore for MemStore {
        async fn upsert_person(
            &self,
            name: &str,
            _sort_name: Option<&str>,
            _provider_ids: Option<&str>,
            _thumb_url: Option<&str>,
        ) -> DomainResult<i64> {
            let wid = pharos_core::person_wire_id(name);
            let id = i64::from_str_radix(&wid[..15], 16).unwrap_or(0);
            Ok(id)
        }
        async fn link_item_people(
            &self,
            item: MediaId,
            people: &[pharos_core::PersonRef],
        ) -> DomainResult<()> {
            let mut wanted: Vec<pharos_core::PersonRef> = Vec::new();
            for p in people {
                if p.name.trim().is_empty() {
                    continue;
                }
                let role = p.role.as_deref().unwrap_or("").trim();
                let dup = wanted.iter().any(|w| {
                    w.name.trim() == p.name.trim() && w.role.as_deref().unwrap_or("").trim() == role
                });
                if !dup {
                    wanted.push(p.clone());
                }
            }
            self.item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(item, wanted);
            Ok(())
        }
        async fn people_with_counts(&self) -> DomainResult<Vec<pharos_core::PersonCount>> {
            use std::collections::BTreeMap;
            let links = self
                .item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut counts: BTreeMap<String, u32> = BTreeMap::new();
            for people in links.values() {
                for p in people {
                    *counts.entry(p.name.clone()).or_insert(0) += 1;
                }
            }
            Ok(counts
                .into_iter()
                .map(|(name, item_count)| pharos_core::PersonCount {
                    person: pharos_core::Person {
                        id: 0,
                        wire_id: pharos_core::person_wire_id(&name),
                        name,
                        sort_name: None,
                        provider_ids: None,
                        thumb_url: None,
                    },
                    item_count,
                })
                .collect())
        }
        async fn person_by_wire_id(
            &self,
            wire_id: &str,
        ) -> DomainResult<Option<pharos_core::Person>> {
            let links = self
                .item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            for people in links.values() {
                for p in people {
                    if pharos_core::person_wire_id(&p.name) == wire_id {
                        return Ok(Some(pharos_core::Person {
                            id: 0,
                            wire_id: wire_id.to_string(),
                            name: p.name.clone(),
                            sort_name: None,
                            provider_ids: p.provider_ids.clone(),
                            thumb_url: p.thumb.clone(),
                        }));
                    }
                }
            }
            Ok(None)
        }
        async fn item_ids_for_person(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
            let links = self
                .item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut ids: Vec<MediaId> = links
                .iter()
                .filter(|(_, people)| {
                    people
                        .iter()
                        .any(|p| pharos_core::person_wire_id(&p.name) == wire_id)
                })
                .map(|(id, _)| *id)
                .collect();
            ids.sort_unstable();
            Ok(ids)
        }
        async fn people_needing_images(
            &self,
            limit: i64,
        ) -> DomainResult<Vec<pharos_core::Person>> {
            let links = self
                .item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut seen = std::collections::BTreeSet::new();
            let mut out = Vec::new();
            for people in links.values() {
                for p in people {
                    let has_http = p
                        .thumb
                        .as_deref()
                        .is_some_and(|t| t.starts_with("http://") || t.starts_with("https://"));
                    if has_http || !seen.insert(p.name.clone()) {
                        continue;
                    }
                    out.push(pharos_core::Person {
                        id: 0,
                        wire_id: pharos_core::person_wire_id(&p.name),
                        name: p.name.clone(),
                        sort_name: None,
                        provider_ids: p.provider_ids.clone(),
                        thumb_url: p.thumb.clone(),
                    });
                }
            }
            out.truncate(limit.max(0) as usize);
            Ok(out)
        }
        async fn people_for_item(
            &self,
            item: MediaId,
        ) -> DomainResult<Vec<pharos_core::ItemPerson>> {
            let links = self
                .item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut out: Vec<pharos_core::ItemPerson> = links
                .get(&item)
                .map(|people| {
                    people
                        .iter()
                        .map(|p| pharos_core::ItemPerson {
                            name: p.name.clone(),
                            wire_id: pharos_core::person_wire_id(&p.name),
                            role: p.role.clone(),
                            character: p.character.clone(),
                            kind: p.kind,
                            sort_order: p.sort_order,
                            thumb_url: p.thumb.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by(|a, b| {
                a.sort_order
                    .is_none()
                    .cmp(&b.sort_order.is_none())
                    .then(a.sort_order.cmp(&b.sort_order))
                    .then(a.name.cmp(&b.name))
            });
            Ok(out)
        }
    }

    // LIB-C3 — minimal in-memory StudioStore so the scanner's
    // link-on-write path can be exercised without the sqlx store.
    impl StudioStore for MemStore {
        async fn upsert_studio(&self, name: &str) -> DomainResult<i64> {
            let wid = pharos_core::studio_wire_id(name);
            let id = i64::from_str_radix(&wid[..15], 16).unwrap_or(0);
            Ok(id)
        }
        async fn link_item_studios(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
            let mut wanted: Vec<String> = Vec::new();
            for n in names {
                let t = n.trim();
                if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                    wanted.push(t.to_string());
                }
            }
            self.item_studios
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(item, wanted);
            Ok(())
        }
        async fn studios_with_counts(&self) -> DomainResult<Vec<pharos_core::StudioCount>> {
            use std::collections::BTreeMap;
            let links = self
                .item_studios
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut counts: BTreeMap<String, u32> = BTreeMap::new();
            for names in links.values() {
                for n in names {
                    *counts.entry(n.clone()).or_insert(0) += 1;
                }
            }
            Ok(counts
                .into_iter()
                .map(|(name, item_count)| pharos_core::StudioCount {
                    studio: pharos_core::Studio {
                        id: 0,
                        wire_id: pharos_core::studio_wire_id(&name),
                        name,
                    },
                    item_count,
                })
                .collect())
        }
        async fn item_ids_for_studio(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
            let links = self
                .item_studios
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut ids: Vec<MediaId> = links
                .iter()
                .filter(|(_, names)| {
                    names
                        .iter()
                        .any(|n| pharos_core::studio_wire_id(n) == wire_id)
                })
                .map(|(id, _)| *id)
                .collect();
            ids.sort_unstable();
            Ok(ids)
        }
        async fn studios_for_item(&self, item: MediaId) -> DomainResult<Vec<pharos_core::Studio>> {
            let links = self
                .item_studios
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut out: Vec<pharos_core::Studio> = links
                .get(&item)
                .map(|names| {
                    names
                        .iter()
                        .map(|name| pharos_core::Studio {
                            id: 0,
                            wire_id: pharos_core::studio_wire_id(name),
                            name: name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        }
    }

    // LIB-C6 — minimal in-memory TagStore so the scanner's link-on-write
    // path (NFO <tag> + filename quality tokens) can be exercised without
    // the sqlx store. Tags are a Vec per item; add/remove mutate in place.
    impl TagStore for MemStore {
        async fn upsert_tag(&self, name: &str) -> DomainResult<i64> {
            let wid = pharos_core::tag_wire_id(name);
            let id = i64::from_str_radix(&wid[..15], 16).unwrap_or(0);
            Ok(id)
        }
        async fn link_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
            let mut wanted: Vec<String> = Vec::new();
            for n in names {
                let t = n.trim();
                if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                    wanted.push(t.to_string());
                }
            }
            self.item_tags
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .insert(item, wanted);
            Ok(())
        }
        async fn add_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<u64> {
            let mut links = self
                .item_tags
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let cur = links.entry(item).or_default();
            let mut added = 0u64;
            for n in names {
                let t = n.trim();
                if !t.is_empty() && !cur.iter().any(|w| w == t) {
                    cur.push(t.to_string());
                    added += 1;
                }
            }
            Ok(added)
        }
        async fn remove_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<u64> {
            let mut links = self
                .item_tags
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let Some(cur) = links.get_mut(&item) else {
                return Ok(0);
            };
            let drop: Vec<&str> = names.iter().map(|n| n.trim()).collect();
            let before = cur.len();
            cur.retain(|t| !drop.contains(&t.as_str()));
            Ok((before - cur.len()) as u64)
        }
        async fn tags_with_counts(&self) -> DomainResult<Vec<pharos_core::TagCount>> {
            use std::collections::BTreeMap;
            let links = self
                .item_tags
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut counts: BTreeMap<String, u32> = BTreeMap::new();
            for names in links.values() {
                for n in names {
                    *counts.entry(n.clone()).or_insert(0) += 1;
                }
            }
            Ok(counts
                .into_iter()
                .map(|(name, item_count)| pharos_core::TagCount {
                    tag: pharos_core::Tag {
                        id: 0,
                        wire_id: pharos_core::tag_wire_id(&name),
                        name,
                    },
                    item_count,
                })
                .collect())
        }
        async fn item_ids_for_tag(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
            let links = self
                .item_tags
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut ids: Vec<MediaId> = links
                .iter()
                .filter(|(_, names)| names.iter().any(|n| pharos_core::tag_wire_id(n) == wire_id))
                .map(|(id, _)| *id)
                .collect();
            ids.sort_unstable();
            Ok(ids)
        }
        async fn tags_for_item(&self, item: MediaId) -> DomainResult<Vec<pharos_core::Tag>> {
            let links = self
                .item_tags
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut out: Vec<pharos_core::Tag> = links
                .get(&item)
                .map(|names| {
                    names
                        .iter()
                        .map(|name| pharos_core::Tag {
                            id: 0,
                            wire_id: pharos_core::tag_wire_id(name),
                            name: name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        }
    }

    // LIB-C5 — minimal in-memory CollectionStore so the scanner's
    // link-on-write path (NFO <set>) can be exercised without the sqlx
    // store. Membership is an ordered Vec per name (curated sort_order).
    impl CollectionStore for MemStore {
        async fn upsert_collection(
            &self,
            name: &str,
            _kind: Option<&str>,
            _overview: Option<&str>,
        ) -> DomainResult<i64> {
            self.collections
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .entry(name.to_string())
                .or_default();
            let wid = pharos_core::collection_wire_id(name);
            Ok(i64::from_str_radix(&wid[..15], 16).unwrap_or(0))
        }
        async fn link_item_collections(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
            let mut map = self
                .collections
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            for n in names {
                let t = n.trim();
                if t.is_empty() {
                    continue;
                }
                let members = map.entry(t.to_string()).or_default();
                if !members.contains(&item) {
                    members.push(item);
                }
            }
            Ok(())
        }
        async fn collections_with_counts(&self) -> DomainResult<Vec<pharos_core::CollectionCount>> {
            use std::collections::BTreeMap;
            let map = self
                .collections
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let ordered: BTreeMap<String, usize> =
                map.iter().map(|(n, m)| (n.clone(), m.len())).collect();
            Ok(ordered
                .into_iter()
                .map(|(name, count)| pharos_core::CollectionCount {
                    collection: pharos_core::Collection {
                        id: 0,
                        wire_id: pharos_core::collection_wire_id(&name),
                        name,
                        kind: "boxset".into(),
                        overview: None,
                    },
                    item_count: count as u32,
                })
                .collect())
        }
        async fn collection_by_wire_id(
            &self,
            wire_id: &str,
        ) -> DomainResult<Option<pharos_core::Collection>> {
            let map = self
                .collections
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            Ok(map
                .keys()
                .find(|n| pharos_core::collection_wire_id(n) == wire_id)
                .map(|name| pharos_core::Collection {
                    id: 0,
                    wire_id: pharos_core::collection_wire_id(name),
                    name: name.clone(),
                    kind: "boxset".into(),
                    overview: None,
                }))
        }
        async fn collection_items(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
            let map = self
                .collections
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            Ok(map
                .iter()
                .find(|(n, _)| pharos_core::collection_wire_id(n) == wire_id)
                .map(|(_, members)| members.clone())
                .unwrap_or_default())
        }
        async fn create_collection(
            &self,
            name: &str,
            item_ids: &[MediaId],
        ) -> DomainResult<pharos_core::Collection> {
            self.upsert_collection(name, None, None).await?;
            let wid = pharos_core::collection_wire_id(name);
            self.add_collection_items(&wid, item_ids).await?;
            Ok(pharos_core::Collection {
                id: 0,
                wire_id: wid,
                name: name.to_string(),
                kind: "boxset".into(),
                overview: None,
            })
        }
        async fn add_collection_items(
            &self,
            wire_id: &str,
            item_ids: &[MediaId],
        ) -> DomainResult<Option<u64>> {
            let mut map = self
                .collections
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let Some(name) = map
                .keys()
                .find(|n| pharos_core::collection_wire_id(n) == wire_id)
                .cloned()
            else {
                return Ok(None);
            };
            let members = map.entry(name).or_default();
            let mut added = 0u64;
            for id in item_ids {
                if !members.contains(id) {
                    members.push(*id);
                    added += 1;
                }
            }
            Ok(Some(added))
        }
        async fn remove_collection_items(
            &self,
            wire_id: &str,
            item_ids: &[MediaId],
        ) -> DomainResult<Option<u64>> {
            let mut map = self
                .collections
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let Some(name) = map
                .keys()
                .find(|n| pharos_core::collection_wire_id(n) == wire_id)
                .cloned()
            else {
                return Ok(None);
            };
            let members = map.entry(name).or_default();
            let before = members.len();
            members.retain(|id| !item_ids.contains(id));
            Ok(Some((before - members.len()) as u64))
        }
    }

    #[derive(Clone, Default)]
    struct FakeProber {
        calls: Arc<AtomicUsize>,
        force_fail_for: Option<String>,
        // LIB-C4 — when set, every probed item carries this genre string
        // so the scanner's link-on-write path can be asserted.
        genre: Option<String>,
        // When set, audio probes carry this embedded track title so the
        // "embedded title beats filename stem" path can be asserted.
        audio_title: Option<String>,
        // B90 — when set, every probe carries this embedded synopsis so the
        // embedded-tag provider → Overview wiring can be asserted.
        synopsis: Option<String>,
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
                    title: if kind == MediaKind::Audio {
                        self.audio_title.clone()
                    } else {
                        None
                    },
                    synopsis: self.synopsis.clone(),
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
    async fn audio_title_prefers_embedded_tag_over_filename_stem() {
        // The file is named by track number; its embedded title is the real
        // song name. The scanner must use the embedded title (B-music: songs
        // were all inheriting the album-folder name from the filename/NFO).
        let td = TempDir::new().unwrap();
        touch(td.path(), "02 Stars.flac").await;
        let prober = FakeProber {
            audio_title: Some("Something Got Me Started".into()),
            ..Default::default()
        };
        let items = FsScanner::new(prober).scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, MediaKind::Audio);
        assert_eq!(items[0].title, "Something Got Me Started");
    }

    #[tokio::test]
    async fn audio_without_embedded_title_falls_back_to_stem() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "02 Stars.flac").await;
        // No embedded title → the filename stem is the fallback.
        let items = FsScanner::new(FakeProber::default())
            .scan(td.path())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "02 Stars");
    }

    #[tokio::test]
    async fn embedded_synopsis_becomes_overview() {
        // B90 — a movie with no sidecar NFO but an embedded synopsis tag must
        // surface it as the item Overview, proving the embedded-tag provider is
        // wired into the default resolver and its output persists through merge.
        // Enrichment runs on the store-backed scan path (not the bare walk).
        let td = TempDir::new().unwrap();
        write_file(td.path(), "movie.mkv", b"aaaa").await;
        let prober = FakeProber {
            synopsis: Some("An embedded plot.".into()),
            ..Default::default()
        };
        let store = MemStore::default();
        FsScanner::new(prober)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].metadata.overview.as_deref(),
            Some("An embedded plot.")
        );
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
            ..Default::default()
        };
        let s = FsScanner::new(prober.clone());
        let items = s.scan(td.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "good");
        assert_eq!(prober.calls.load(Ordering::SeqCst), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn walk_skips_unreadable_entry_and_indexes_the_rest() {
        // A transient per-entry walk error (an *arr import moving a file
        // mid-scan → EPERM/ENOENT on one path, or a subtree we can't read)
        // must NOT abort the whole library scan (V6). Reproduce it with a
        // mode-000 subdirectory: walkdir can't descend, yielding an Err entry
        // exactly like the churning-file case seen on the live NFS mount.
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        write_file(td.path(), "good.mkv", b"aaaa").await;
        let locked = td.path().join("locked");
        tokio::fs::create_dir(&locked).await.unwrap();
        write_file(&locked, "hidden.mkv", b"bbbb").await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Running as root ignores the perms, so the walk error can't be
        // provoked — skip rather than assert a false pass.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        let result = s.scan_into(td.path(), &store).await;
        // Restore perms before asserting so TempDir cleanup works even if the
        // assertions below fail.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = result.expect("one unreadable entry must not fail the scan");
        assert_eq!(
            outcome.added.len(),
            1,
            "the readable file should be indexed"
        );
        assert_eq!(store.list().await.unwrap().len(), 1);
        assert_eq!(store.list().await.unwrap()[0].title, "good");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn incomplete_walk_does_not_prune_transiently_unreadable_files() {
        // A file indexed on a clean scan must NOT be swept on a later scan
        // whose walk is incomplete (an unreadable subtree hides it). Otherwise
        // a momentary NFS EPERM / an *arr import in flight makes present media
        // vanish from the library until the next scan re-adds it.
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        write_file(td.path(), "show/ep.mkv", b"aaaa").await;

        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());

        // Scan 1 (clean): ep.mkv is indexed.
        let first = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(first.added.len(), 1);
        assert_eq!(store.list().await.unwrap().len(), 1);

        // Make the containing dir unreadable so scan 2's walk can't list
        // ep.mkv — an incomplete listing, not a real deletion.
        let show = td.path().join("show");
        std::fs::set_permissions(&show, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&show).is_ok() {
            // Running as root — perms ignored, can't provoke; skip.
            std::fs::set_permissions(&show, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        // Scan 2 (incomplete walk): the sweep must be skipped, so ep.mkv's row
        // survives even though it wasn't seen this pass.
        let second = s.scan_into(td.path(), &store).await;
        std::fs::set_permissions(&show, std::fs::Permissions::from_mode(0o755)).unwrap();

        let second = second.expect("incomplete walk must not error");
        assert!(
            second.removed.is_empty(),
            "must not sweep on an incomplete walk"
        );
        assert_eq!(
            store.list().await.unwrap().len(),
            1,
            "the still-present file must not be pruned by an incomplete walk"
        );
    }

    #[tokio::test]
    async fn scan_populates_item_genres_join_from_probe_genre() {
        // LIB-C4 — scanning a file whose probe carries a comma/pipe
        // separated genre string links it into the item_genres join, and
        // the genre's wire id resolves the item back.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "movie.mkv", b"aaaa").await;
        let prober = FakeProber {
            genre: Some("Action, Sci-Fi".into()),
            ..Default::default()
        };
        let s = FsScanner::new(prober);
        let store = MemStore::default();
        s.scan_into(td.path(), &store).await.unwrap();
        let item_id = store.list().await.unwrap()[0].id;

        use pharos_core::{genre_wire_id, GenreStore};
        let rows = store.genres_with_counts().await.unwrap();
        let names: Vec<&str> = rows.iter().map(|g| g.genre.name.as_str()).collect();
        assert_eq!(names, vec!["Action", "Sci-Fi"]);
        let ids = store
            .item_ids_for_genre(&genre_wire_id("Action"))
            .await
            .unwrap();
        assert_eq!(ids, vec![item_id]);
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
    async fn force_scan_reprobes_unchanged_files() {
        // A forced rescan re-probes every file even when its (mtime,size)
        // signature is unchanged — the recovery path for a probe-schema change
        // (e.g. newly extracted MediaAttachments) that the incremental skip
        // would otherwise never pick up.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "movie.mkv", b"aaaa").await;
        write_file(td.path(), "song.flac", b"bbbbbb").await;

        let prober = FakeProber::default();
        let store = MemStore::default();

        let first = FsScanner::new(prober.clone())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(first.probed(), 2, "first scan probes both");

        let before = prober.calls.load(Ordering::SeqCst);
        // An incremental rescan would skip both; --force re-probes both.
        let out = FsScanner::new(prober.clone())
            .with_force(true)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(out.skipped, 0, "force scan skips nothing");
        assert_eq!(out.updated.len(), 2, "force re-probes both as updates");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst) - before,
            2,
            "both files re-probed under force"
        );
    }

    #[tokio::test]
    async fn stale_probe_schema_version_forces_reprobe() {
        // #10 — a file unchanged on disk but last probed under an OLDER
        // `PROBE_SCHEMA_VERSION` (or a pre-migration row that defaulted to 0)
        // must be re-probed so a new probe field backfills automatically —
        // resumably, without `--force`.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "movie.mkv", b"aaaa").await;
        let prober = FakeProber::default();
        let store = MemStore::default();
        let s = FsScanner::new(prober.clone());

        // First scan probes + stamps the current version.
        assert_eq!(s.scan_into(td.path(), &store).await.unwrap().probed(), 1);

        // Force the stored version stale (simulate an old / pre-migration row).
        let id = stable_id(&td.path().join("movie.mkv"));
        store
            .states
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .probe_schema_version = 0;

        // Rescan, file unchanged: the stale version alone triggers a re-probe.
        let before = prober.calls.load(Ordering::SeqCst);
        let out = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(out.skipped, 0, "stale-version row must not be skipped");
        assert_eq!(out.updated.len(), 1, "stale-version row is re-probed");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst) - before,
            1,
            "exactly one re-probe"
        );

        // Now current again → a subsequent unchanged scan skips.
        assert_eq!(
            s.scan_into(td.path(), &store).await.unwrap().skipped,
            1,
            "row at current version skips when unchanged"
        );
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
            ..Default::default()
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
    async fn io_gate_caps_probe_concurrency_below_the_fan_out() {
        // Adaptive backpressure — a shared I/O gate must throttle the probe
        // fan-out to at most the gate's available permits, EVEN when
        // `probe_concurrency` is set higher. This is what lets the live server
        // shrink the gate (parking permits) during playback to pace a
        // background rescan down without pausing it. Gate = 2 permits, fan-out
        // = 8 ⇒ observed peak must never exceed 2.
        let td = TempDir::new().unwrap();
        for i in 0..12 {
            write_file(td.path(), &format!("f{i}.mkv"), &[b'x'; 1]).await;
        }
        let gate = Arc::new(tokio::sync::Semaphore::new(2));
        let prober = ConcurrencyProber::default();
        let s = FsScanner::new(prober.clone())
            .with_probe_concurrency(8)
            .with_io_gate(gate);
        let store = MemStore::default();

        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 12, "all twelve imported");

        let peak = prober.peak.load(Ordering::SeqCst);
        assert!(
            peak <= 2,
            "io-gate of 2 permits must cap concurrency below the fan-out of 8 (peak {peak})"
        );
        assert!(peak >= 1, "work must still make progress (peak {peak})");
        assert_eq!(prober.in_flight.load(Ordering::SeqCst), 0, "gate released");
    }

    #[tokio::test]
    async fn io_gate_throttled_to_one_serialises_probes() {
        // The regulator's playback-active state parks all but one permit; with
        // exactly one permit the scan must serialise (peak 1) yet still finish
        // every file — proving playback never blocks the scan outright, only
        // throttles it.
        let td = TempDir::new().unwrap();
        for i in 0..6 {
            write_file(td.path(), &format!("f{i}.mkv"), &[b'x'; 1]).await;
        }
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let prober = ConcurrencyProber::default();
        let s = FsScanner::new(prober.clone())
            .with_probe_concurrency(8)
            .with_io_gate(gate);
        let store = MemStore::default();

        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(
            outcome.added.len(),
            6,
            "all six imported even fully throttled"
        );
        assert_eq!(
            prober.peak.load(Ordering::SeqCst),
            1,
            "a single-permit gate serialises the probe stream"
        );
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
    fn parse_episode_covers_season_episode_forms() {
        // (name, expected (season, episode)) — the explicit season+episode
        // conventions Jellyfin's first-pass expressions handle.
        let cases = [
            ("My.Show.S02E07.mkv", (Some(2), 7)),
            ("show s1e1.mp4", (Some(1), 1)),
            ("Series_S12E07_HDTV.mkv", (Some(12), 7)),
            ("Show S01 E02.mkv", (Some(1), 2)),
            ("Show.S01.E02.mkv", (Some(1), 2)),
            ("Show S01xE02.mkv", (Some(1), 2)),
            ("Show 1x02.mkv", (Some(1), 2)),
            ("Show 01x02.mkv", (Some(1), 2)),
            ("Show Season 3 Episode 4.mkv", (Some(3), 4)),
            // Multi-episode: the START episode is what orders the file.
            ("Show.S01E02-E03.mkv", (Some(1), 2)),
            ("Show.S01E02E03.mkv", (Some(1), 2)),
        ];
        for (name, want) in cases {
            assert_eq!(parse_episode_from_name(name), Some(want), "{name}");
        }
    }

    #[test]
    fn parse_episode_covers_absolute_and_marker_forms() {
        // Episode-only markers → season None (defaulted to 1 upstream).
        assert_eq!(
            parse_episode_from_name("Code Geass E01.mkv"),
            Some((None, 1))
        );
        assert_eq!(parse_episode_from_name("Show EP14.mkv"), Some((None, 14)));
        assert_eq!(
            parse_episode_from_name("Series Episode 3.mkv"),
            Some((None, 3))
        );
        // Absolute anime numbering — the dominant fansub dash convention.
        for (name, want) in [
            ("[Group] Code Geass - 01 [1080p].mkv", 1u32),
            ("Code Geass Lelouch of the Rebellion - 12 [BD].mkv", 12),
            ("Code Geass - 25v2.mkv", 25),
            ("[SubsPlease] Show - 08 (1080p) [ABCD].mkv", 8),
        ] {
            assert_eq!(parse_episode_from_name(name), Some((None, want)), "{name}");
        }
        // Bracketed absolute + whole-file-is-a-number.
        assert_eq!(parse_episode_from_name("Show [12].mkv"), Some((None, 12)));
        assert_eq!(parse_episode_from_name("07.mkv"), Some((None, 7)));
    }

    #[test]
    fn parse_episode_rejects_false_positives() {
        // Resolution / codec / year must NOT read as an episode.
        assert_eq!(parse_episode_from_name("Movie - 1080p x264.mkv"), None);
        assert_eq!(parse_episode_from_name("Concert - 2006.mkv"), None);
        assert_eq!(parse_episode_from_name("Plain Movie (2019).mkv"), None);
        assert_eq!(parse_episode_from_name("The Big Short 2015.mkv"), None);
    }

    #[test]
    fn parse_series_info_uses_absolute_number_and_defaults_season_one() {
        // Anime folder, no season dir, no SxxEyy — the absolute number drives
        // ordering and the season defaults to 1 so the episode groups cleanly.
        let p = Path::new("/anime/Code Geass/[Group] Code Geass - 07 [1080p].mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Code Geass");
        assert_eq!(info.episode_number, Some(7));
        assert_eq!(info.season_number, Some(1));
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

    // LIB-C11 — two distinct shows sharing a name must yield the SAME
    // series_name but DIFFERENT series_folder + year so their synthesised
    // wire ids diverge and their episodes don't interleave.
    #[test]
    fn same_name_shows_get_distinct_folders_and_years() {
        let p80 = Path::new("/tv/Cosmos (1980)/Season 01/S01E01.mkv");
        let p14 = Path::new("/tv/Cosmos (2014)/Season 01/S01E01.mkv");
        let i80 = parse_series_info(p80).expect("series info 1980");
        let i14 = parse_series_info(p14).expect("series info 2014");

        // Display name is identical (the bare show name)…
        assert_eq!(i80.series_name, "Cosmos");
        assert_eq!(i14.series_name, "Cosmos");
        // …but the folder identity + parsed year differ.
        assert_eq!(i80.series_folder.as_deref(), Some("/tv/Cosmos (1980)"));
        assert_eq!(i14.series_folder.as_deref(), Some("/tv/Cosmos (2014)"));
        assert_eq!(i80.series_year, Some(1980));
        assert_eq!(i14.series_year, Some(2014));
        assert_ne!(i80.series_key(), i14.series_key());
    }

    #[test]
    fn folder_year_only_parsed_from_trailing_parenthesised_4digit() {
        // No year marker → None.
        let p = Path::new("/tv/Firefly/Season 01/S01E01.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Firefly");
        assert_eq!(info.series_folder.as_deref(), Some("/tv/Firefly"));
        assert_eq!(info.series_year, None);
        // Falls back to the bare name as identity key when no folder year.
        assert_eq!(info.series_key(), "/tv/Firefly");

        // Parenthesised non-year doesn't masquerade as a year.
        assert_eq!(parse_folder_year("Show (Uncut)"), None);
        assert_eq!(parse_folder_year("Show (1)"), None);
        assert_eq!(parse_folder_year("Cosmos (1980)"), Some(1980));
        assert_eq!(parse_folder_year("The Office (US) (2005)"), Some(2005));
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

    // ---- LIB-D7: resolver wired into scan_into ------------------------

    /// Read a single item out of the MemStore by its path-derived id.
    fn item_by_path(store: &MemStore, path: &Path) -> MediaItem {
        let id = stable_id(path);
        store
            .inner
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_else(|| panic!("no item for {}", path.display()))
    }

    /// The genre names linked to the item at `path` (via the item_genres
    /// mirror — the same join /Genres + ParentId resolve against).
    fn genres_for(store: &MemStore, path: &Path) -> Vec<String> {
        let id = stable_id(path);
        store
            .item_genres
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn scan_populates_metadata_genre_and_artwork_from_local_sources() {
        // A movie with a sibling Kodi NFO (overview / year / rating / genre /
        // tmdbid) and a poster.jpg sidecar. After a scan the EPIC C fields
        // are populated from the NFO, the genre is linked, and a Primary
        // artwork row points at the poster — the whole local-first pipeline.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Movie (2017).mkv", b"video-bytes").await;
        write_file(
            td.path(),
            "Movie (2017).nfo",
            br#"<?xml version="1.0"?>
<movie>
  <title>The Real Movie</title>
  <plot>An epic synopsis.</plot>
  <year>2017</year>
  <rating>8.4</rating>
  <genre>Science Fiction</genre>
  <uniqueid type="tmdb">12345</uniqueid>
</movie>"#,
        )
        .await;
        write_file(td.path(), "poster.jpg", b"\xff\xd8\xff\xe0jpeg").await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 1, "the movie should be imported");

        let movie_path = td.path().join("Movie (2017).mkv");
        let item = item_by_path(&store, &movie_path);

        // NFO scalars merged onto MediaMetadata.
        assert_eq!(item.metadata.overview.as_deref(), Some("An epic synopsis."));
        assert_eq!(item.metadata.production_year, Some(2017));
        assert_eq!(item.metadata.community_rating, Some(8.4));
        assert_eq!(item.metadata.provider_ids.tmdb.as_deref(), Some("12345"));
        // NFO <title> wins over the filename stem.
        assert_eq!(item.title, "The Real Movie");

        // Genre linked into the item_genres join.
        assert_eq!(genres_for(&store, &movie_path), vec!["Science Fiction"]);

        // Primary artwork row points at the poster sidecar.
        let id = stable_id(&movie_path);
        let art = store.artwork_for(id).await.unwrap();
        let primary = art
            .iter()
            .find(|(role, _, _)| role == "Primary")
            .expect("a Primary artwork row");
        assert_eq!(primary.1, "local");
        assert!(
            primary.2.ends_with("poster.jpg"),
            "Primary should point at poster.jpg, got {}",
            primary.2
        );
    }

    #[tokio::test]
    async fn scan_persists_people_from_nfo_cast_and_crew() {
        // LIB-C2 — a movie with a Kodi NFO carrying <actor> (name / role /
        // order / thumb) + <director> + <credits>. After a scan the cast &
        // crew are linked into item_people, the person wire id resolves the
        // item back, and the headshot is persisted on the person row.
        use pharos_core::{person_wire_id, PersonKind, PersonStore};
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Movie (2017).mkv", b"video-bytes").await;
        write_file(
            td.path(),
            "Movie (2017).nfo",
            br#"<?xml version="1.0"?>
<movie>
  <title>The Real Movie</title>
  <director>Lana Wachowski</director>
  <credits>David Mitchell</credits>
  <actor>
    <name>Keanu Reeves</name>
    <role>Neo</role>
    <order>0</order>
    <thumb>http://img/keanu.jpg</thumb>
  </actor>
  <actor>
    <name>Carrie-Anne Moss</name>
    <role>Trinity</role>
    <order>1</order>
  </actor>
</movie>"#,
        )
        .await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 1);

        let movie_path = td.path().join("Movie (2017).mkv");
        let id = stable_id(&movie_path);

        // All four credits linked into item_people, in NFO order
        // (actors first by sort_order; crew with no order trail).
        let credits = store.people_for_item(id).await.unwrap();
        let names: Vec<&str> = credits.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[0], "Keanu Reeves", "order 0 first");
        assert_eq!(names[1], "Carrie-Anne Moss", "order 1 second");
        assert!(names.contains(&"Lana Wachowski"));
        assert!(names.contains(&"David Mitchell"));

        // Structured kind + character round-trip.
        let keanu = credits.iter().find(|c| c.name == "Keanu Reeves").unwrap();
        assert_eq!(keanu.kind, PersonKind::Actor);
        assert_eq!(keanu.character.as_deref(), Some("Neo"));
        let dir = credits.iter().find(|c| c.name == "Lana Wachowski").unwrap();
        assert_eq!(dir.kind, PersonKind::Director);
        let wri = credits.iter().find(|c| c.name == "David Mitchell").unwrap();
        assert_eq!(wri.kind, PersonKind::Writer);

        // ParentId pivot: person wire id → the crediting item.
        let ids = store
            .item_ids_for_person(&person_wire_id("Keanu Reeves"))
            .await
            .unwrap();
        assert_eq!(ids, vec![id]);

        // Headshot persisted on the person row.
        let p = store
            .person_by_wire_id(&person_wire_id("Keanu Reeves"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.thumb_url.as_deref(), Some("http://img/keanu.jpg"));
    }

    #[tokio::test]
    async fn scan_persists_studios_from_nfo() {
        // LIB-C3 — a movie with a Kodi NFO carrying two <studio> tags. After
        // a scan the studios are linked into item_studios, the studio wire id
        // resolves the item back, and the item's studios project name-ordered.
        use pharos_core::{studio_wire_id, StudioStore};
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Movie (2017).mkv", b"video-bytes").await;
        write_file(
            td.path(),
            "Movie (2017).nfo",
            br#"<?xml version="1.0"?>
<movie>
  <title>The Real Movie</title>
  <studio>Warner Bros.</studio>
  <studio>Village Roadshow</studio>
</movie>"#,
        )
        .await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 1);

        let movie_path = td.path().join("Movie (2017).mkv");
        let id = stable_id(&movie_path);

        // Both studios linked into item_studios, projected name-ordered.
        let studios = store.studios_for_item(id).await.unwrap();
        let names: Vec<&str> = studios.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Village Roadshow", "Warner Bros."]);
        // The projected studio carries the wire id the DTO emits.
        assert_eq!(studios[1].wire_id, studio_wire_id("Warner Bros."));

        // /Studios list reflects both with a per-studio item count.
        let counts = store.studios_with_counts().await.unwrap();
        let wb = counts
            .iter()
            .find(|c| c.studio.name == "Warner Bros.")
            .expect("Warner Bros. studio");
        assert_eq!(wb.item_count, 1);
        assert_eq!(wb.studio.wire_id, studio_wire_id("Warner Bros."));

        // ParentId pivot: studio wire id → the tagged item.
        let ids = store
            .item_ids_for_studio(&studio_wire_id("Warner Bros."))
            .await
            .unwrap();
        assert_eq!(ids, vec![id]);
        // An unknown wire id resolves to nothing.
        let none = store.item_ids_for_studio("deadbeef").await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn scan_persists_tags_from_nfo_and_filename() {
        // LIB-C6 — a movie whose filename carries a quality token (1080p,
        // BluRay) and whose NFO carries a <tag>. After a scan all the tags
        // are linked into item_tags, project name-ordered onto the item,
        // resolve back via the tag wire id, and count in tags_with_counts.
        use pharos_core::{tag_wire_id, TagStore};
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Movie (2017) [1080p] BluRay.mkv", b"video-bytes").await;
        write_file(
            td.path(),
            "Movie (2017) [1080p] BluRay.nfo",
            br#"<?xml version="1.0"?>
<movie>
  <title>The Real Movie</title>
  <tag>cyberpunk</tag>
</movie>"#,
        )
        .await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 1);

        let movie_path = td.path().join("Movie (2017) [1080p] BluRay.mkv");
        let id = stable_id(&movie_path);

        // The NFO <tag> + filename quality/source tokens all land in
        // item_tags. We assert the NFO tag + at least the quality tokens
        // the filename provider extracts are present and name-ordered.
        let tags = store.tags_for_item(id).await.unwrap();
        let names: Vec<String> = tags.iter().map(|t| t.name.clone()).collect();
        assert!(
            names.contains(&"cyberpunk".to_string()),
            "NFO <tag> persisted, got {names:?}"
        );
        assert!(
            names.contains(&"1080p".to_string()),
            "filename quality token persisted, got {names:?}"
        );
        assert!(
            names.contains(&"BluRay".to_string()),
            "filename source token persisted, got {names:?}"
        );
        // Name-ordered (tags_for_item sorts).
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tags project name-ordered");

        // ParentId pivot: the NFO tag's wire id → the tagged item.
        let ids = store
            .item_ids_for_tag(&tag_wire_id("cyberpunk"))
            .await
            .unwrap();
        assert_eq!(ids, vec![id]);
        // /Tags list reflects the NFO tag with a per-tag item count.
        let counts = store.tags_with_counts().await.unwrap();
        let cp = counts
            .iter()
            .find(|c| c.tag.name == "cyberpunk")
            .expect("cyberpunk tag");
        assert_eq!(cp.item_count, 1);
        assert_eq!(cp.tag.wire_id, tag_wire_id("cyberpunk"));
        // An unknown wire id resolves to nothing.
        assert!(store.item_ids_for_tag("deadbeef").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scan_persists_collections_from_nfo() {
        // LIB-C5 — a movie with a Kodi NFO carrying a <set> tag. After a
        // scan the box set is created, the item is a member, and the
        // collection wire id resolves the item back via collection_items.
        // Covers BOTH the flat <set>Name</set> and the nested
        // <set><name>Name</name></set> Jellyfin form.
        use pharos_core::{collection_wire_id, CollectionStore};
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Movie (2017).mkv", b"video-bytes").await;
        write_file(
            td.path(),
            "Movie (2017).nfo",
            br#"<?xml version="1.0"?>
<movie>
  <title>The Real Movie</title>
  <set><name>The Matrix Collection</name></set>
  <collection>Wachowski Films</collection>
</movie>"#,
        )
        .await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(outcome.added.len(), 1);

        let movie_path = td.path().join("Movie (2017).mkv");
        let id = stable_id(&movie_path);

        // Both the nested <set><name> and the flat <collection> are box sets.
        let counts = store.collections_with_counts().await.unwrap();
        let names: Vec<&str> = counts.iter().map(|c| c.collection.name.as_str()).collect();
        assert_eq!(names, vec!["The Matrix Collection", "Wachowski Films"]);

        // ParentId pivot: collection wire id → the member item.
        let members = store
            .collection_items(&collection_wire_id("The Matrix Collection"))
            .await
            .unwrap();
        assert_eq!(members, vec![id]);
        assert_eq!(
            store
                .collection_items(&collection_wire_id("Wachowski Films"))
                .await
                .unwrap(),
            vec![id]
        );
        // An unknown wire id resolves to nothing.
        assert!(store.collection_items("deadbeef").await.unwrap().is_empty());

        // Idempotent: a rescan keeps a single membership (no dupes).
        let _ = s.scan_into(td.path(), &store).await.unwrap();
        let members = store
            .collection_items(&collection_wire_id("The Matrix Collection"))
            .await
            .unwrap();
        assert_eq!(members, vec![id], "rescan keeps one membership");
    }

    #[tokio::test]
    async fn malformed_nfo_still_imports_from_probe_and_scan_completes() {
        // V6 — a truncated / malformed NFO makes the NFO provider return Err;
        // the resolver logs + skips it and the item is still imported from
        // probe data with empty descriptive metadata. The scan never aborts.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Broken.mkv", b"video").await;
        write_file(td.path(), "Broken.nfo", b"<movie><title>oops</tit").await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        let outcome = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(
            outcome.added.len(),
            1,
            "scan completed and imported the file"
        );

        let item = item_by_path(&store, &td.path().join("Broken.mkv"));
        // No metadata bled through from the broken NFO. (Filename provider
        // may still set a title/year, but the NFO scalars must be absent.)
        assert_eq!(item.metadata.overview, None);
        assert_eq!(item.metadata.community_rating, None);
        assert!(item.metadata.provider_ids.is_empty());
    }

    #[tokio::test]
    async fn file_with_no_sidecars_imports_with_empty_metadata() {
        // No NFO, no sidecar art: the item imports from probe data with empty
        // descriptive metadata and no artwork rows. (The filename provider
        // derives a title/year but leaves overview/ratings/ids empty.)
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Lonely.mkv", b"video").await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        s.scan_into(td.path(), &store).await.unwrap();

        let movie_path = td.path().join("Lonely.mkv");
        let item = item_by_path(&store, &movie_path);
        assert_eq!(item.metadata.overview, None);
        assert_eq!(item.metadata.community_rating, None);
        assert!(item.metadata.provider_ids.is_empty());

        let art = store.artwork_for(stable_id(&movie_path)).await.unwrap();
        assert!(
            art.is_empty(),
            "no sidecars => no artwork rows, got {art:?}"
        );
    }

    #[tokio::test]
    async fn probe_genre_and_nfo_genre_union_without_duplicates() {
        // The probe's embedded genre and the NFO <genre> tags are UNIONed
        // (probe first), de-duped — an overlap is not double-linked.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Mix.mkv", b"video").await;
        write_file(
            td.path(),
            "Mix.nfo",
            br#"<movie><genre>Drama</genre><genre>Action</genre></movie>"#,
        )
        .await;

        let prober = FakeProber {
            genre: Some("Drama, Thriller".into()),
            ..Default::default()
        };
        let s = FsScanner::new(prober);
        let store = MemStore::default();
        s.scan_into(td.path(), &store).await.unwrap();

        let mut g = genres_for(&store, &td.path().join("Mix.mkv"));
        g.sort();
        // Drama (probe ∩ NFO) appears once; union = {Drama, Thriller, Action}.
        assert_eq!(g, vec!["Action", "Drama", "Thriller"]);
    }

    #[tokio::test]
    async fn empty_resolver_keeps_probe_data_only() {
        // with_resolver(empty) — no providers — leaves metadata/title/artwork
        // exactly as the probe produced them (no NFO/sidecar/filename merge).
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Plain (2020).mkv", b"video").await;
        write_file(
            td.path(),
            "Plain (2020).nfo",
            br#"<movie><title>Ignored</title><plot>nope</plot></movie>"#,
        )
        .await;

        let s = FsScanner::new(FakeProber::default()).with_resolver(MetadataResolver::new());
        let store = MemStore::default();
        s.scan_into(td.path(), &store).await.unwrap();

        let movie_path = td.path().join("Plain (2020).mkv");
        let item = item_by_path(&store, &movie_path);
        // Title stays the raw filename stem; NFO is never read.
        assert_eq!(item.title, "Plain (2020)");
        assert_eq!(item.metadata.overview, None);
        assert!(store
            .artwork_for(stable_id(&movie_path))
            .await
            .unwrap()
            .is_empty());
    }
}
