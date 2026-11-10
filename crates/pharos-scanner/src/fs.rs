//! Recursive media filesystem scan. Generic over `Prober` (V12).
//! Walk lives in `spawn_blocking` — never parks async runtime (V5).

use futures_util::stream::StreamExt;
use pharos_core::{
    AlternateMediaSource, ArtworkSource, CollectionStore, Detect3DSource, DomainError,
    DomainResult, Fingerprint, GenreStore, MediaId, MediaItem, MediaKind, MediaStore,
    MetadataRequest, MetadataResult, PersonStore, Prober, ScanOutcome, Scanner, SeriesInfo,
    StudioStore, TagStore, Video3DFormat,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::Instrument as _;
use xxhash_rust::xxh3::xxh3_64;

use crate::metadata::{
    book::BookMetadataProvider, embedded::EmbeddedTagProvider, filename::FilenameProvider,
    nfo::NfoProvider, sidecar::SidecarArtworkProvider, MetadataResolver,
};

pub const DEFAULT_EXTENSIONS: &[&str] = &[
    // Media — handed to the prober.
    "mkv", "mp4", "mov", "avi", "webm", "m4v", "flac", "mp3", "opus", "m4a", "ogg", "wav",
    // 004-books — books, which must be WALKED but never probed. `probe_one`
    // branches on `book::is_book_path` before touching the prober: an epub is
    // not a media container, ffprobe/libav fails on it, and V6 makes a probe
    // miss write nothing — so a book handed to ffmpeg is an item that never
    // exists. Kept in one list because "does pharos walk this file" is a single
    // question; "can a client read it" is a different one, answered by
    // `BookFormat::readable_by_client`.
    "epub", "pdf", "cbz", "cbr", "cbt", "cb7", "mobi", "azw", "azw3",
];

/// Which schema version governs whether an already-indexed `path` is stale.
///
/// 005-kindle-conversion — the incremental scan skips a file whose
/// `(mtime, size)` are unchanged AND whose stored version still matches. A
/// book's row is produced by the book parser and never by the prober, so
/// comparing it against `PROBE_SCHEMA_VERSION` ties two unrelated pipelines
/// together in both directions, and both are bad:
///
/// * bumping the probe version to refresh 142 books re-probes ~14,000 video
///   files through ffmpeg over NFS;
/// * NOT bumping it leaves every book permanently skipped, which is how Kindle
///   conversion shipped to a library where nothing changed — all 75 Kindle
///   rows already carried the current probe version, so the scan skipped them
///   and the feature was inert on exactly the files it was written for.
///
/// The store's `mark_seen` picks the same version from the row's `kind`, so
/// the value written and the value compared cannot drift.
pub fn expected_schema_version(path: &Path) -> i64 {
    if crate::book::is_book_path(path) {
        pharos_core::BOOK_SCHEMA_VERSION
    } else {
        pharos_core::PROBE_SCHEMA_VERSION
    }
}

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

/// Probed items written per `put_batch`. Big enough that a commit's fsync is
/// amortised over a few hundred rows, small enough that the write lock is
/// held for tens of milliseconds, not seconds, and a crash loses little work.
const DEFAULT_PUT_BATCH_SIZE: usize = 256;

/// How long a partly filled batch may wait for company before it is written.
const DEFAULT_PUT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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

/// Live progress of an in-flight [`FsScanner::scan_into`] over one root:
/// `processed` groups resolved so far out of `total` groups discovered by the
/// walk. `processed == total` on completion. Feeds the Jellyfin-shaped scan
/// progress surface (`ScheduledTasks.CurrentProgressPercentage` + the
/// `RefreshProgress` socket message) so a client can render a live scan bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgress {
    /// Groups (primary + its alternate editions) fully resolved so far —
    /// skipped-unchanged, moved/rebound, or probed-and-written.
    pub processed: u64,
    /// Total groups the directory walk discovered under this root. Known
    /// up front (the walk completes before probing begins), so the fraction
    /// is meaningful from the first callback.
    pub total: u64,
}

impl ScanProgress {
    /// Fraction complete as a `0.0..=100.0` percentage. `total == 0` (an empty
    /// root) reports `100.0` — there is nothing to do, so the scan is done.
    #[must_use]
    pub fn percent(self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        (self.processed as f64 / self.total as f64) * 100.0
    }
}

/// Sink for [`ScanProgress`] callbacks. `Arc<dyn Fn>` so the scanner stays
/// cheap to `Clone` and the server can fan a single closure (which stamps a
/// shared atomic + broadcasts a socket message) across every root.
pub type ProgressSink = std::sync::Arc<dyn Fn(ScanProgress) + Send + Sync>;

/// One scan at a time in the process (B313/V253). Every scan stamps the rows
/// it sees with its own scan id and then sweeps the unstamped ones under its
/// root, so two scans over the same files (a library inside a mixed root, each
/// with its own task) would sweep each other's stamps and drop files still on
/// disk. Held for the whole of [`FsScanner::scan_into`] and of a watcher batch.
static SCAN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Waits for any other scan in the process to finish; the scan holds the guard.
pub(crate) async fn scan_exclusively() -> tokio::sync::MutexGuard<'static, ()> {
    SCAN_GATE.lock().await
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
    /// Probed items are written with one `put_batch` per this many (one
    /// transaction instead of a commit per file). `1` keeps per-item writes.
    put_batch_size: usize,
    /// A partly filled batch is written once its oldest item has waited this
    /// long, so new files appear and progress moves on a slow-probing scan.
    put_flush_interval: std::time::Duration,
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
    /// Optional live-progress sink. When set, [`scan_into`](Self::scan_into)
    /// invokes it as groups resolve (throttled to integer-percent changes) so
    /// a caller can surface a scan bar. `None` (CLI scans, tests that don't
    /// assert progress) = no callbacks, zero overhead.
    progress: Option<ProgressSink>,
    /// Cooperative cancellation flag. When set and flipped `true` mid-scan, the
    /// walk stops queueing probes, drains no more of the probe stream, and —
    /// critically — SKIPS the mark-and-sweep deletion pass (a partial run has
    /// seen only a subset of what's on disk, so sweeping would prune every
    /// not-yet-visited file). `None` (CLI/tests) = never cancels.
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Remembers files that failed to probe, so a scan does not re-read them
    /// every pass. A failed probe writes no row, so the incremental
    /// `(mtime, size)` skip can never apply to it and the file is re-opened
    /// forever — see [`crate::probe_memo`]. `None` (CLI scans, tests) keeps the
    /// old behaviour of retrying everything.
    probe_memo: Option<Arc<crate::probe_memo::ProbeMemo>>,
    /// Spec 033 — whether a full pass writes the series split it decides, or
    /// only logs it (`[metadata] series_identity_repair`).
    series_identity_repair: crate::series_split::RepairMode,
    /// 004-books (T063) — where a book's extracted cover BYTES are written.
    ///
    /// A cover lives INSIDE the book file, so unlike every other artwork source
    /// there is no path on the media filesystem to record. The bytes have to
    /// land somewhere first, and the only writable place is the server's image
    /// cache — which this crate deliberately does not depend on (V12: the
    /// scanner's domain logic stays testable without one). So the destination
    /// arrives as a sink the server implements over `ImageCache::upload`.
    ///
    /// `None` (CLI scans, tests) means covers are still READ — the classify
    /// counter must report the same rate wherever the scan runs — and then
    /// dropped, so no `Primary` row is written and no tag is advertised.
    cover_sink: Option<Arc<dyn CoverSink>>,
}

/// 004-books (T063) — destination for a book cover's bytes.
///
/// Implemented by the server over the existing on-disk image cache, so a book's
/// cover is stored, scaled, ETagged and served by exactly the same machinery as
/// a sidecar poster or a downloaded provider image. Boxed future rather than
/// AFIT because this is used as a trait object.
pub trait CoverSink: Send + Sync {
    /// Persist `bytes` as this item's Primary artwork and return the absolute
    /// path they were written to, which the caller records via `set_artwork`.
    fn store_cover<'a>(
        &'a self,
        item_id: MediaId,
        bytes: Vec<u8>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<std::path::PathBuf>> + Send + 'a>,
    >;
}

impl<P: Prober> std::fmt::Debug for FsScanner<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsScanner")
            .field("extensions", &self.extensions.len())
            .field("rate_limit", &self.rate_limit)
            .field("probe_concurrency", &self.probe_concurrency)
            .field("put_batch_size", &self.put_batch_size)
            .field("metadata_providers", &self.resolver.provider_count())
            .finish_non_exhaustive()
    }
}

/// Spec 033 (V940) — reset an episode's provider-derived fields (title,
/// overview, premiere date, community rating, provider ids) to what local
/// evidence alone says: the default resolver (NFO ▸ sidecar ▸ filename) over
/// its file, as a scan reads it, so an enrichment that only fills empty
/// fields starts from local data again. Probe data, linked entities (genres,
/// people) and artwork are left as stored. Reads files: call it off the
/// async reactor.
pub async fn reset_to_local_metadata(item: &mut MediaItem) {
    let meta = {
        let req = MetadataRequest {
            path: &item.path,
            kind: item.kind,
            probe: &item.probe,
            series: item.series.as_ref(),
        };
        default_resolver().resolve(&req).await
    };
    if let Some(stem) = item.path.file_stem().and_then(|s| s.to_str()) {
        item.title = stem.to_string();
    }
    let md = &mut item.metadata;
    md.overview = None;
    md.premiere_date = None;
    md.community_rating = None;
    md.provider_ids = pharos_core::ProviderIds::default();
    let _ = merge_metadata_into_item(item, meta);
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
        // 004-books — the epub OPF / ComicInfo.xml reader, at the same rung as
        // embedded container tags: a curated .nfo still wins any scalar, and a
        // filename still supplies a title the file itself did not carry.
        .with_provider(BookMetadataProvider::new())
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
            put_batch_size: DEFAULT_PUT_BATCH_SIZE,
            put_flush_interval: DEFAULT_PUT_FLUSH_INTERVAL,
            resolver: Arc::new(default_resolver()),
            force: false,
            io_gate: None,
            progress: None,
            cancel: None,
            probe_memo: None,
            series_identity_repair: crate::series_split::RepairMode::DryRun,
            cover_sink: None,
        }
    }

    pub fn with_extensions(prober: P, exts: impl IntoIterator<Item = String>) -> Self {
        Self {
            prober,
            extensions: exts.into_iter().collect(),
            rate_limit: std::time::Duration::ZERO,
            probe_concurrency: default_probe_concurrency(),
            put_batch_size: DEFAULT_PUT_BATCH_SIZE,
            put_flush_interval: DEFAULT_PUT_FLUSH_INTERVAL,
            resolver: Arc::new(default_resolver()),
            force: false,
            io_gate: None,
            progress: None,
            cancel: None,
            probe_memo: None,
            series_identity_repair: crate::series_split::RepairMode::DryRun,
            cover_sink: None,
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

    /// Attach a probe-failure memo so files that cannot be probed are skipped
    /// until they change on disk. See [`crate::probe_memo`] for why a repeated
    /// failure is not merely wasteful.
    ///
    /// `--force` still overrides it: an operator asking for a full re-probe
    /// means every file, including the ones already known to be broken.
    pub fn with_probe_memo(mut self, memo: Arc<crate::probe_memo::ProbeMemo>) -> Self {
        self.probe_memo = Some(memo);
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

    /// Attach a live-progress sink invoked as the scan resolves groups. The
    /// scanner throttles calls to integer-percent changes (plus a final
    /// `processed == total`), so the sink fires at most ~101 times per root
    /// regardless of library size. The server wires this to a shared atomic +
    /// the `RefreshProgress` / `ScheduledTasksInfo` socket broadcasts; CLI
    /// scans and most tests leave it unset.
    pub fn with_progress(mut self, sink: ProgressSink) -> Self {
        self.progress = Some(sink);
        self
    }

    /// Attach a cooperative cancellation flag. Flip it `true` (from another
    /// task) and the in-flight [`scan_into`](Self::scan_into) stops at its next
    /// checkpoint (per group in phase 1, per probe result in phase 3) and skips
    /// the deletion sweep so a partial run never prunes unvisited files. The
    /// server wires this to the `DELETE /ScheduledTasks/Running/{id}` flag.
    /// Spec 033 — write the series split a full pass decides
    /// ([`RepairMode::Apply`](crate::series_split::RepairMode::Apply)) or only
    /// log it (the default).
    pub fn with_series_identity_repair(mut self, mode: crate::series_split::RepairMode) -> Self {
        self.series_identity_repair = mode;
        self
    }

    pub fn with_cancel(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    /// 004-books (T063) — where extracted book covers are written. The server
    /// wires this to the image cache; without it covers are read (so the
    /// classify counter stays truthful) and discarded.
    pub fn with_cover_sink(mut self, sink: Arc<dyn CoverSink>) -> Self {
        self.cover_sink = Some(sink);
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

    /// 004-books (T063) — extract this item's cover and record it as Primary
    /// artwork.
    ///
    /// Called after `put`, because `set_artwork` writes a row keyed on an item
    /// that must already exist. Deliberately goes through `set_artwork` and NOT
    /// `put`: `put` does not maintain the denormalised `has_primary_art`
    /// (B155), so a cover written the other way would be on disk, servable, and
    /// never advertised — present but invisible.
    ///
    /// Best-effort throughout (V6): a cover that cannot be read or written
    /// leaves the book indexed and cover-less, which is a supported state.
    /// Takes the id/kind/path rather than the item because `put` consumes the
    /// item and this must run after it.
    async fn store_book_cover<S: MediaStore>(
        &self,
        store: &S,
        id: MediaId,
        kind: MediaKind,
        path: &Path,
    ) {
        if kind != MediaKind::Book {
            return;
        }
        // Read on every book, sink or no sink. The extractor is what records
        // the cover verdict, and SC-003's rate must mean the same thing whether
        // the scan ran in the server or from the CLI. Blocking archive IO goes
        // off the reactor (V5).
        let owned = path.to_path_buf();
        let Ok(Some(bytes)) =
            tokio::task::spawn_blocking(move || crate::book::read_book_cover(&owned)).await
        else {
            return;
        };
        let Some(sink) = self.cover_sink.as_ref() else {
            return;
        };
        match sink.store_cover(id, bytes).await {
            Ok(stored) => {
                if let Err(e) = store
                    .set_artwork(id, "Primary", "local", &stored.to_string_lossy())
                    .await
                {
                    tracing::warn!(
                        media.id = id,
                        media.path = %path.display(),
                        error = %e,
                        "book cover written but not recorded; it will not be advertised"
                    );
                }
            }
            Err(e) => tracing::warn!(
                media.id = id,
                media.path = %path.display(),
                error = %e,
                "book cover extracted but could not be cached"
            ),
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

    /// Write probed items in batches of up to `size` (clamped to at least 1;
    /// `1` writes each item on its own).
    pub fn with_put_batch_size(mut self, size: usize) -> Self {
        self.put_batch_size = size.max(1);
        self
    }

    /// Longest a partly filled batch waits before it is written.
    pub fn with_put_flush_interval(mut self, interval: std::time::Duration) -> Self {
        self.put_flush_interval = interval;
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
    ///
    /// Records `pharos_scan_duration_seconds{outcome}` and
    /// `pharos_scan_items_total{result}` for every run.
    #[tracing::instrument(skip(self, store), fields(root = %root.display()))]
    pub async fn scan_into<
        S: MediaStore
            + GenreStore
            + PersonStore
            + StudioStore
            + CollectionStore
            + TagStore
            + pharos_core::SeriesIdentityStore,
    >(
        &self,
        root: &Path,
        store: &S,
    ) -> DomainResult<ScanOutcome> {
        let _exclusive = scan_exclusively().await;
        forget_show_nfos();
        let started = std::time::Instant::now();
        // Boxed: `scan_run`'s future is a deep stack of store futures, and
        // inlining it into every caller's async block pushes the server's
        // release-build layout computation past the recursion limit.
        let result = Box::pin(self.scan_run(root, store)).await;
        let cancelled = self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst));
        let outcome_label = match (&result, cancelled) {
            (Err(_), _) => "error",
            (Ok(_), true) => "cancelled",
            (Ok(_), false) => "ok",
        };
        metrics::histogram!("pharos_scan_duration_seconds", "outcome" => outcome_label)
            .record(started.elapsed().as_secs_f64());
        if let Ok(out) = &result {
            for (label, n) in [
                ("added", out.added.len()),
                ("updated", out.updated.len()),
                ("skipped", out.skipped),
                ("removed", out.removed.len()),
            ] {
                metrics::counter!("pharos_scan_items_total", "result" => label)
                    .increment(u64::try_from(n).unwrap_or(u64::MAX));
            }
        }
        result
    }

    async fn scan_run<
        S: MediaStore
            + GenreStore
            + PersonStore
            + StudioStore
            + CollectionStore
            + TagStore
            + pharos_core::SeriesIdentityStore,
    >(
        &self,
        root: &Path,
        store: &S,
    ) -> DomainResult<ScanOutcome> {
        let scan_id = store.begin_scan(root).await?;
        let WalkOutcome {
            files: paths,
            errors: walk_errors,
        } = walk(root.to_path_buf(), self.extensions.clone())
            .instrument(tracing::info_span!("scan.walk"))
            .await?;
        let series_folders = crate::series_split::folders_of(&paths);
        let root_prefix = root.to_string_lossy().into_owned();
        // The split runs before the fold, so a split folder's groups are
        // separate shows before their copies are grouped (V894).
        let series_plan = self
            .plan_series(store, &series_folders, &root_prefix)
            .instrument(tracing::info_span!("scan.series_plan"))
            .await;
        let guard = match &series_plan {
            Some((plan, ctx)) => {
                let keys = FoldGuard::match_keys(&series_folders, plan);
                let ambiguous = store.ambiguous_series(&keys).await.unwrap_or_else(|e| {
                    tracing::warn!(
                        error = %e,
                        root = %root_prefix,
                        "could not read which shows are flagged; folding them by split alone",
                    );
                    std::collections::HashSet::new()
                });
                FoldGuard::new(
                    &series_folders,
                    plan,
                    ctx,
                    ambiguous,
                    self.series_identity_repair,
                )
            }
            None => FoldGuard::default(),
        };
        let groups = group_by_edition(paths);
        let facts = copy_facts(store, &copy_candidates(&groups))
            .instrument(tracing::info_span!("scan.copy_facts"))
            .await;
        let mut tally = CopyTally::default();
        let groups = group_copies_guarded(groups, &facts, &guard, &mut tally);
        tally.publish();
        // A file now grouped under another may still hold its own row from an
        // earlier pass; that row is swept, so its viewers' state moves first.
        let folded = folded_copies(&groups);
        let mut outcome = ScanOutcome::default();
        let mut seen = 0i64;

        // Live-progress plumbing. `total` is known up front (the walk finished
        // before we probe), so the fraction is meaningful from the first
        // callback. The emitter throttles to integer-percent changes so a
        // 13k-item library fires the sink ~101 times, not 13k. `None` sink
        // (CLI/tests) compiles to a cheap no-op branch.
        let total = groups.len() as u64;
        let progress_sink = self.progress.clone();
        let mut last_pct: i64 = -1;
        let mut emit = |processed: u64| {
            if let Some(sink) = &progress_sink {
                // `checked_div` folds the empty-root (total == 0) guard into the
                // division: no divisor → nothing to do → 100%.
                let pct = (processed.min(total) * 100)
                    .checked_div(total)
                    .map_or(100, |p| p as i64);
                if pct != last_pct {
                    last_pct = pct;
                    sink(ScanProgress { processed, total });
                }
            }
        };
        // A visible 0% the moment the task starts (before the first probe).
        emit(0);

        // Cooperative cancellation: checked per group (phase 1) and per probe
        // result (phase 3). Cloned so the check never re-borrows `self` while
        // the probe stream does.
        let cancel_flag = self.cancel.clone();
        let is_cancelled = || {
            cancel_flag
                .as_ref()
                .is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst))
        };

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
            /// `stable_id(primary)`: the probe memo's key.
            id: MediaId,
            /// The id the probed row is written under: the row already
            /// holding `primary` when there is one, else `id` (B257).
            row: MediaId,
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
        async {
            for (i, (primary, alts)) in groups.into_iter().enumerate() {
                // Stop queueing work the moment cancellation is requested; the sweep
                // below is skipped too, so the partial run prunes nothing.
                if is_cancelled() {
                    break;
                }
                // Groups fully resolved in phase 1 so far = those iterated that did
                // NOT defer to the probe stream (`pending`). Probed groups are
                // counted later, as their stream result lands (phase 3).
                emit((i as u64).saturating_sub(pending.len() as u64));
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
                // A file that FAILED to probe writes no row at all, so nothing below
                // can skip it: `scan_state` is `None`, the move-detection
                // fingerprint runs a whole-file hash, and the probe then re-reads
                // it in full — on every pass, forever. The memo is the only record
                // that the attempt happened. `--force` still overrides it.
                if !self.force
                    && self
                        .probe_memo
                        .as_ref()
                        .is_some_and(|m| m.is_known_bad(id, sig))
                {
                    // Deliberately NOT added to `seen_batch`: there is no row to
                    // mark seen, and inventing one would put an unplayable item in
                    // the library (V6).
                    outcome.skipped += 1;
                    continue;
                }

                let (row, existing_state) = match store.scan_state(id).await? {
                    Some(state) => (id, Some(state)),
                    // B257 — a seeded row, or one rebound after a move, holds
                    // this path under another id.
                    None => match store.id_at_path(&primary).await? {
                        Some(held) => (held, store.scan_state(held).await?),
                        None => (id, None),
                    },
                };
                if let Some(state) = existing_state {
                    // Existing-by-path: `row` holds this exact path. Skip the probe iff the
                    // signature still matches; otherwise re-probe + put. `--force`
                    // (self.force) bypasses the skip so every file is re-probed.
                    if let Some((mtime, size)) = sig {
                        if !self.force
                            && state.file_mtime == mtime
                            && state.file_size == size
                            && state.probe_schema_version == expected_schema_version(&primary)
                        {
                            // B289 — the primary file is unchanged, but the editions
                            // grouped under it live on its row too. A deleted, added
                            // or resized edition must re-put the row, or its
                            // alternate_sources and has_3d go stale.
                            match edition_drift_for(store, row, state.has_editions, &alts).await? {
                                None if !kind_drift(store, row, &primary).await => {
                                    seen_batch.push((row, mtime, size));
                                    seen += 1;
                                    outcome.skipped += 1;
                                    continue;
                                }
                                None => {}
                                Some((reason, edition)) => {
                                    record_edition_drift(
                                        row, &primary, reason, &edition, "reprobed",
                                    );
                                }
                            }
                        }
                    }
                    pending.push(Pending {
                        primary,
                        alts,
                        id,
                        row,
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
                            row,
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
            Ok::<(), DomainError>(())
        }
        .instrument(tracing::info_span!("scan.stat_skip"))
        .await?;

        // Every group that skipped in phase 1 is now resolved; the remainder
        // (`pending`) is what the probe stream still has to work through.
        let phase1_done = total - pending.len() as u64;
        emit(phase1_done);
        let mut probed_done: u64 = 0;

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
                    (p.id, p.row, p.sig, p.existed, fp, item, meta)
                }
            })
            .buffer_unordered(self.probe_concurrency);

        // Probed items wait here so `put_batch` can write them in one
        // transaction; the per-item follow-up writes (joins, artwork, mark_seen)
        // need the row to exist, so they run right after each flush.
        let mut batch_items: Vec<MediaItem> = Vec::new();
        let mut batch_followups: Vec<WrittenItem> = Vec::new();
        let mut batch_started: Option<tokio::time::Instant> = None;
        async {
            loop {
                // A partly filled batch waits at most `put_flush_interval` for
                // company, so a slow-probing scan still lands its files.
                let next = match batch_started {
                    None => stream.next().await,
                    Some(started) => {
                        match tokio::time::timeout_at(
                            started + self.put_flush_interval,
                            stream.next(),
                        )
                        .await
                        {
                            Ok(next) => next,
                            Err(_) => {
                                self.flush_probe_writes(
                                    store,
                                    scan_id,
                                    &mut batch_items,
                                    &mut batch_followups,
                                    &mut outcome,
                                    &mut seen,
                                )
                                .await?;
                                batch_started = None;
                                emit(phase1_done + probed_done);
                                continue;
                            }
                        }
                    }
                };
                let Some((probed_id, row, sig, existed, fp, item, meta)) = next else {
                    break;
                };
                // Cancellation: stop draining the probe stream. Rows already
                // probed are flushed below (the sweep is skipped, so nothing is
                // pruned). Remaining in-flight probes are dropped with the stream.
                if is_cancelled() {
                    break;
                }
                // Count every probe result (hit or miss) so progress reaches 100%
                // even when a file fails to probe (V6 — a miss writes nothing).
                // Progress only counts items that are written, so a buffered
                // item is not reported until its batch lands.
                probed_done += 1;
                emit(
                    phase1_done + probed_done
                        - batch_items.len() as u64
                        - u64::from(item.is_some()),
                );
                // Record the outcome either way, so the next pass can skip a file
                // that cannot be probed and re-try one that has been replaced. This
                // is the only place a probe MISS leaves any trace — a miss writes
                // no row (V6), which is what made it repeat forever.
                if let Some(memo) = self.probe_memo.as_ref() {
                    match &item {
                        Some(_) => memo.forget(probed_id),
                        None => memo.record(probed_id, sig),
                    }
                }
                let Some(mut item) = item else { continue };
                item.id = row;
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
                // Captured before `put` consumes the item; the cover write below
                // needs them and must run AFTER the row exists.
                let (item_kind, item_path) = (item.kind, item.path.clone());
                batch_items.push(item);
                batch_followups.push(WrittenItem {
                    item_id,
                    item_kind,
                    item_path,
                    genres,
                    artwork,
                    people,
                    studios,
                    collections,
                    tags,
                    sig,
                    fp,
                    existed,
                    stale_match: None,
                });
                if batch_items.len() >= self.put_batch_size {
                    self.flush_probe_writes(
                        store,
                        scan_id,
                        &mut batch_items,
                        &mut batch_followups,
                        &mut outcome,
                        &mut seen,
                    )
                    .await?;
                    batch_started = None;
                } else if batch_started.is_none() {
                    batch_started = Some(tokio::time::Instant::now());
                }
                emit(phase1_done + probed_done - batch_items.len() as u64);
            }
            // The stream is drained (or the run was cancelled): write what is
            // still buffered, so a cancelled run keeps the files it probed.
            self.flush_probe_writes(
                store,
                scan_id,
                &mut batch_items,
                &mut batch_followups,
                &mut outcome,
                &mut seen,
            )
            .await?;
            Ok::<(), DomainError>(())
        }
        .instrument(tracing::info_span!("scan.probe_write"))
        .await?;
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
        // A cancelled run has visited only a subset of disk, exactly like an
        // incomplete walk — sweeping would prune every not-yet-seen file. Skip
        // it; a later full scan reconciles genuine deletions.
        let cancelled = is_cancelled();
        // A fold is final only on the pass whose sweep removes the copy's row;
        // folding on a pass that keeps the row would repeat the merge next
        // pass and re-mark watched what a viewer since marked unwatched.
        if !cancelled && walk_errors == 0 {
            self.settle_series_identity(store, series_plan.as_ref(), &root_prefix)
                .instrument(tracing::info_span!("scan.series_identity"))
                .await;
            settle_copies(store, scan_id, &folded).await?;
        } else if !folded.is_empty() {
            tracing::info!(
                scan_id,
                cancelled,
                walk_errors,
                copies = folded.len(),
                "kept copies' rows unfolded: this pass does not sweep",
            );
        }
        let swept = if cancelled {
            tracing::info!(scan_id, "scan cancelled mid-run; skipping deletion sweep");
            Vec::new()
        } else if walk_errors == 0 {
            store
                .sweep_unseen(scan_id, &root_prefix)
                .instrument(tracing::info_span!("scan.sweep"))
                .await?
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
        // Terminal 100% — guarantees a final `processed == total` callback even
        // for an empty root (no groups, no probes) so the consumer's task state
        // always lands on complete. Suppressed on cancel: a stopped run did not
        // reach 100%, so don't claim it did.
        if !cancelled {
            emit(total);
        }
        // Persist what this pass learned about unreadable files. Written here
        // rather than by the caller because the scanner owns the memo and is
        // the only thing that knows a pass has ended — both the CLI scan and
        // the server's watcher get it without either having to remember.
        //
        // A cancelled run still saves: what it recorded is true, and losing it
        // only means re-reading those files next time.
        if let Some(memo) = self.probe_memo.as_ref() {
            match memo.save() {
                Ok(()) => tracing::info!(
                    known_bad = memo.len(),
                    path = %memo.path().display(),
                    "probe-failure memo saved; these files are skipped until they change on disk",
                ),
                // Never fatal: the memo is an optimisation, and failing a whole
                // scan because a cache file could not be written would trade a
                // slow scan for no scan.
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %memo.path().display(),
                    "could not save the probe-failure memo; unreadable files will be re-probed next pass",
                ),
            }
        }
        Ok(outcome)
    }

    /// Spec 033 — the split plan for a walk's episode folders (V893), from
    /// the store's overrides and stored matches. A failed read is logged and
    /// plans nothing, so keys stay as stored and the copy fold runs unguarded.
    async fn plan_series<S: pharos_core::SeriesIdentityStore>(
        &self,
        store: &S,
        folders: &std::collections::BTreeMap<String, Vec<(PathBuf, Option<u32>)>>,
        root_prefix: &str,
    ) -> Option<(
        crate::series_split::SeriesPlan,
        crate::series_split::PlanContext,
    )> {
        use crate::series_split as split;
        match split::context(store, folders).await {
            Ok(ctx) => Some((split::plan(folders, &ctx, unix_now()), ctx)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    root = root_prefix,
                    mode = self.series_identity_repair.label(),
                    "series identity pass failed; keys left as stored",
                );
                None
            }
        }
    }

    /// Write (or, in dry run, log) each file's series key from the split
    /// plan. Runs only on a complete walk: a partial listing could see one
    /// group of a split folder and undo the split.
    async fn settle_series_identity<S: pharos_core::SeriesIdentityStore>(
        &self,
        store: &S,
        planned: Option<&(
            crate::series_split::SeriesPlan,
            crate::series_split::PlanContext,
        )>,
        root_prefix: &str,
    ) {
        use crate::series_split as split;
        let mode = self.series_identity_repair;
        let Some((plan, _)) = planned else {
            return;
        };
        let result = split::reconcile(store, plan, root_prefix, mode, unix_now())
            .await
            .map(|changed| (changed, plan.splits.len()));
        // Never fatal: the copy fold and the deletion sweep still run.
        match result {
            Ok((0, _)) => {}
            Ok((changed, splits)) => {
                tracing::info!(changed, splits, "series keys written");
            }
            Err(e) => tracing::warn!(
                error = %e,
                root = root_prefix,
                mode = mode.label(),
                "series identity pass failed; keys left as stored",
            ),
        }
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
        S: MediaStore
            + GenreStore
            + PersonStore
            + StudioStore
            + CollectionStore
            + TagStore
            + pharos_core::SeriesIdentityStore,
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
        let (row, existing_state) = match store.scan_state(id).await? {
            Some(state) => (id, Some(state)),
            // B257 — a seeded row, or one rebound after a move, holds this
            // path under another id.
            None => match store.id_at_path(path).await? {
                Some(held) => (held, store.scan_state(held).await?),
                None => (id, None),
            },
        };
        if let Some(state) = existing_state {
            if let Some((mtime, size)) = sig {
                if !self.force && state.file_mtime == mtime && state.file_size == size {
                    store.mark_seen(row, scan_id, mtime, size).await?;
                    return Ok(PathUpdate::Skipped);
                }
            }
            // Signature changed (or unreadable) — re-probe + put.
            return self
                .probe_put_one(path, store, scan_id, sig, None, Some(row))
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
                self.probe_put_one(path, store, scan_id, sig, fp, None)
                    .await
            }
        }
    }

    /// Write the buffered probe results with one `put_batch`, then run each
    /// item's follow-up writes (their rows exist now).
    async fn flush_probe_writes<
        S: MediaStore
            + GenreStore
            + PersonStore
            + StudioStore
            + CollectionStore
            + TagStore
            + pharos_core::SeriesIdentityStore,
    >(
        &self,
        store: &S,
        scan_id: i64,
        items: &mut Vec<MediaItem>,
        followups: &mut Vec<WrittenItem>,
        outcome: &mut ScanOutcome,
        seen: &mut i64,
    ) -> DomainResult<()> {
        // A probe can wait here for up to `put_flush_interval`, and the
        // watcher and other scans write the same rows without coordinating
        // with this buffer. If the file changed since it was stat'd, this
        // probe describes bytes that are gone and may be older than what a
        // concurrent writer already stored: do not write it. Stamp the row as
        // seen with the OLD signature instead, so the sweep keeps it and the
        // next scan sees the mismatch and probes again.
        let mut fresh_items = Vec::with_capacity(items.len());
        let mut fresh_followups = Vec::with_capacity(followups.len());
        for (item, written) in items.drain(..).zip(followups.drain(..)) {
            if let Some((mtime, size)) = written.sig {
                let now = tokio::fs::metadata(&written.item_path)
                    .await
                    .ok()
                    .map(|m| (mtime_secs(&m), m.len()));
                if now != Some((mtime, size)) {
                    tracing::info!(
                        path = %written.item_path.display(),
                        "file changed while its probe was buffered; not writing the stale probe"
                    );
                    store
                        .mark_seen(written.item_id, scan_id, mtime, size)
                        .await?;
                    outcome.skipped += 1;
                    continue;
                }
            }
            fresh_items.push(item);
            fresh_followups.push(written);
        }
        *items = fresh_items;
        *followups = fresh_followups;
        if items.is_empty() {
            return Ok(());
        }
        for (item, written) in items.iter().zip(followups.iter_mut()) {
            if written.existed {
                if let Ok(prior) = store.get(item.id).await {
                    written.stale_match = stale_match(&prior, item);
                }
            }
        }
        let batch_err = store.put_batch(items).await.err();
        // `put_batch` writes every item it can but reports only the first
        // error, so ask per item which rows exist now: the ones that landed
        // still get their follow-ups and are counted before the error ends
        // the scan, exactly as they were when each item was put on its own.
        let landed = if batch_err.is_some() {
            let mut landed = Vec::with_capacity(items.len());
            for item in items.iter() {
                landed.push(store.put(item.clone()).await.is_ok());
            }
            landed
        } else {
            vec![true; items.len()]
        };
        items.clear();
        for (written, landed) in followups.drain(..).zip(landed) {
            if landed {
                self.finish_written_item(store, scan_id, written, outcome, seen)
                    .await?;
            }
        }
        batch_err.map_or(Ok(()), Err)
    }

    /// Everything a probed item needs once its row exists.
    async fn finish_written_item<
        S: MediaStore
            + GenreStore
            + PersonStore
            + StudioStore
            + CollectionStore
            + TagStore
            + pharos_core::SeriesIdentityStore,
    >(
        &self,
        store: &S,
        scan_id: i64,
        written: WrittenItem,
        outcome: &mut ScanOutcome,
        seen: &mut i64,
    ) -> DomainResult<()> {
        let WrittenItem {
            item_id,
            item_kind,
            item_path,
            genres,
            artwork,
            people,
            studios,
            collections,
            tags,
            sig,
            fp,
            existed,
            stale_match,
        } = written;
        reset_stale_match(store, item_id, &item_path, stale_match).await?;
        // 004-books — a book's cover lives inside the file, so it has no
        // sidecar path to record and is extracted + cached here instead.
        self.store_book_cover(store, item_id, item_kind, &item_path)
            .await;
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
        *seen += 1;
        Ok(())
    }

    /// LIB-A8 — probe a single path and persist it (the create/modify tail
    /// shared by [`update_path`]). `existing` is the row already holding the
    /// path, written over under its own id (B257) and reported as updated;
    /// `fp` is reused when already computed during move-detect.
    #[tracing::instrument(skip(self, store, sig, fp), fields(media.path = %path.display()))]
    async fn probe_put_one<
        S: MediaStore
            + GenreStore
            + PersonStore
            + StudioStore
            + CollectionStore
            + TagStore
            + pharos_core::SeriesIdentityStore,
    >(
        &self,
        path: &Path,
        store: &S,
        scan_id: i64,
        sig: Option<(i64, u64)>,
        fp: Option<Fingerprint>,
        existing: Option<MediaId>,
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
        if let Some(row) = existing {
            item.id = row;
        }
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
        let item_kind = item.kind;
        let stale = match existing {
            Some(_) => store
                .get(item_id)
                .await
                .ok()
                .and_then(|prior| stale_match(&prior, &item)),
            None => None,
        };
        store.put(item).await?;
        reset_stale_match(store, item_id, path, stale).await?;
        // 004-books — the watched create/modify path extracts + caches a book
        // cover the same way a full scan does, or a book added by the watcher
        // would be the only one without a tile.
        self.store_book_cover(store, item_id, item_kind, path).await;
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
        if existing.is_some() {
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
                    let id = unique_edition_id(&item.probe.alternate_sources, &edition);
                    resolve_item_3d_format(&alt_path, item.kind, &mut probe);
                    item.probe.alternate_sources.push(AlternateMediaSource {
                        id,
                        container: probe.container.clone(),
                        video_codec: probe.video_codec.clone(),
                        audio_codec: probe.audio_codec.clone(),
                        bitrate_bps: probe.bitrate_bps,
                        size_bytes: probe.size_bytes,
                        duration_ms: probe.duration_ms,
                        name: Some(edition_display_name(&edition)),
                        video_3d_format: probe
                            .video_3d_format
                            .or_else(|| parse_3d_edition(&edition)),
                        path: alt_path,
                        probe: Some(Box::new(probe)),
                        part: None,
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

    /// 004-books — build a `MediaItem` for a book path, or `None` when the path
    /// is not a book.
    ///
    /// No ffmpeg, no probe, no async: reading a book is a filesystem + parse
    /// operation. The `MediaProbe` stays `default()`, which `MediaItem.probe`
    /// already documents as the "a row can exist without probe data" case, so no
    /// struct gymnastics are needed.
    ///
    /// The title is the filename stem. A book's real title comes from its OPF or
    /// `ComicInfo.xml` via the metadata resolver, which runs later and overrides
    /// this; the stem guarantees FR-007's "no book is ever listed untitled" even
    /// when the file carries no metadata at all.
    fn book_item(&self, path: &Path) -> Option<MediaItem> {
        let book = crate::book::read_book_meta(path)?;
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        // Stat for size only. Everything else on `MediaProbe` describes streams
        // a book does not have.
        let size_bytes = std::fs::metadata(path).ok().map(|m| m.len());
        Some(MediaItem {
            locked: Default::default(),
            id: stable_id(path),
            path: path.to_path_buf(),
            title,
            kind: MediaKind::Book,
            book: Some(book),
            probe: pharos_core::MediaProbe {
                size_bytes,
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
            edition: None,
        })
    }

    #[tracing::instrument(skip(self), fields(media.path = %path.display()))]
    async fn probe_one(&self, path: PathBuf) -> Option<MediaItem> {
        // 004-books / FR-001 — classify BEFORE the prober is reached. This is
        // not an optimisation: `self.prober.probe` fails on an epub, the `Err`
        // arm below writes nothing (V6), and the item would never exist. The
        // ordering IS the feature.
        if let Some(item) = self.book_item(&path) {
            return Some(item);
        }
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
                    // 004-books — a book already landed here via the wildcard
                    // and `stem()` is the RIGHT answer for it (FR-007's filename
                    // fallback), but it was correct by accident. Named
                    // explicitly, and the wildcard dropped, so the next
                    // `MediaKind` variant has to decide rather than inherit.
                    MediaKind::Movie | MediaKind::Episode | MediaKind::Book => stem(),
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
                let kind = if matches!(info.kind, MediaKind::Movie) && is_episode_file(&path) {
                    let reason = if is_episode_path(&path) {
                        "name_or_season_folder"
                    } else {
                        "show_folder_nfo"
                    };
                    tracing::debug!(
                        path = %path.display(),
                        reason,
                        "scanner: video promoted to an episode"
                    );
                    MediaKind::Episode
                } else {
                    info.kind
                };
                // Every probe path (full scan, edition grouping, watcher)
                // goes through here, so 3D resolution happens once.
                resolve_item_3d_format(&path, kind, &mut probe);
                let series = if matches!(kind, MediaKind::Episode) {
                    episode_series_info(&path)
                } else {
                    None
                };
                Some(MediaItem {
                    locked: Default::default(),
                    id: stable_id(&path),
                    path,
                    title,
                    kind,
                    // This is the PROBE path — a media file that went through
                    // ffmpeg. A book never reaches here (it branches before the
                    // prober, T036), so it carries no book metadata.
                    book: None,
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
                    // Set by `set_artwork` when `persist_artwork` records a
                    // local Primary sidecar (it runs after this row is put);
                    // false here so a coverless item stays false.
                    has_primary_art: false,
                    art_version: 0,
                    match_provider: None,
                    match_external_id: None,
                    match_source: None,
                    match_confidence: None,
                    metadata_refreshed_at: None,
                    edition: None,
                })
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "probe failed, skipping");
                None
            }
        }
    }
}

/// What follows a probed item's row write: the entity joins, artwork, the
/// seen-stamp and fingerprint, and the added-vs-updated bookkeeping.
struct WrittenItem {
    item_id: MediaId,
    item_kind: MediaKind,
    item_path: PathBuf,
    genres: Vec<String>,
    artwork: Vec<pharos_core::ArtworkRef>,
    people: Vec<pharos_core::PersonRef>,
    studios: Vec<String>,
    collections: Vec<String>,
    tags: Vec<String>,
    sig: Option<(i64, u64)>,
    fp: Option<Fingerprint>,
    existed: bool,
    /// Why the row's online match no longer fits it ([`stale_match`]).
    stale_match: Option<(&'static str, String)>,
}

/// Clear a row's online match that [`stale_match`] found no longer fits it,
/// so the matcher asks again.
async fn reset_stale_match<S: MediaStore>(
    store: &S,
    id: MediaId,
    path: &Path,
    stale: Option<(&'static str, String)>,
) -> DomainResult<()> {
    let Some((reason, provider)) = stale else {
        return Ok(());
    };
    tracing::info!(
        media.id = %id,
        path = %path.display(),
        reason,
        provider,
        "reset a row's online match that no longer fits it",
    );
    metrics::counter!("pharos_scan_match_reset_total", "reason" => reason).increment(1);
    store
        .set_item_match(id, &provider, "", "none", None, 0)
        .await
}

/// B1024 — why a re-probed row's stored online match no longer fits it, with
/// the provider to reset: its kind changed (a film now read as an episode,
/// V966), or its match came from an NFO id the NFO no longer gives (a set
/// aside NFO, V968). `put` leaves the match columns alone, so without this the
/// old film's record would keep coming back.
fn stale_match(prior: &MediaItem, now: &MediaItem) -> Option<(&'static str, String)> {
    let provider = prior.match_provider.clone()?;
    if prior.kind != now.kind {
        return Some(("kind_changed", provider));
    }
    if prior.match_source.as_deref() != Some("nfo_id") {
        return None;
    }
    let ids = &now.metadata.provider_ids;
    let given = match provider.as_str() {
        "tmdb" => ids.tmdb.as_deref(),
        "tvdb" => ids.tvdb.as_deref(),
        "imdb" => ids.imdb.as_deref(),
        _ => return None,
    };
    (given != prior.match_external_id.as_deref()).then_some(("nfo_id_gone", provider))
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
        let groups = group_by_edition(paths);
        let candidates: Vec<PathBuf> = copy_candidates(&groups)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let sizes = copy_sizes(&candidates).await;
        let groups = group_copies(groups, &sizes, &mut CopyTally::default());
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
    if !is_known_edition(edition) && parse_3d_edition(edition).is_none() {
        return None;
    }
    Some((left.trim(), edition))
}

/// Parse a 3D edition label such as `3D.HSBS` or `3D HSBS` (case-insensitive)
/// into its packing. A bare `3D` carries no packing, so it is not recognised:
/// the player cannot unpack a stereo file without knowing the layout.
pub(crate) fn parse_3d_edition(label: &str) -> Option<Video3DFormat> {
    let label = label.trim();
    if !label.get(..2)?.eq_ignore_ascii_case("3d") {
        return None;
    }
    let tag = label.get(2..)?.strip_prefix(['.', ' '])?;
    Video3DFormat::from_short_tag(tag.trim())
}

/// `MediaSourceInfo.Name` for an edition label. 3D labels are normalised to
/// `3D HSBS` so the version picker reads the same whichever separator the
/// filename used; other editions keep the user's spelling.
fn edition_display_name(edition: &str) -> String {
    match parse_3d_edition(edition) {
        Some(f) => format!("3D {}", f.short_tag()),
        None => edition.to_string(),
    }
}

pub(crate) fn is_known_edition(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    KNOWN_EDITIONS.iter().any(|e| *e == lower)
}

/// [`edition_id_slug`] made unique among the alternates already attached:
/// `X - 3D.HSBS` and `X - 3D HSBS` slug alike, and a repeated id would make
/// the second one unaddressable on PlaybackInfo.
fn unique_edition_id(taken: &[AlternateMediaSource], edition: &str) -> String {
    let base = edition_id_slug(edition);
    let mut id = base.clone();
    let mut n = 2;
    while taken.iter().any(|a| a.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
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

/// Why an unchanged primary no longer qualifies for the incremental skip
/// because the editions grouped under it changed (B289). The `label()` strings
/// are the `reason` values of `pharos_scan_reprobe_total`, a dashboard
/// contract: do not rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReprobeReason {
    /// A stored edition's file is no longer grouped under the primary.
    Removed,
    /// A file is grouped under the primary that the stored row lacks.
    Added,
    /// A stored edition's file is still there but its size differs.
    Changed,
}

impl ReprobeReason {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::Removed, Self::Added, Self::Changed];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Removed => "edition_removed",
            Self::Added => "edition_added",
            Self::Changed => "edition_changed",
        }
    }
}

/// Compare the editions stored on a primary's row with the ones on disk
/// (`disk` is each grouped alternate's path and current size). Returns the
/// first difference and the path it concerns. `AlternateMediaSource` records
/// no mtime, so a changed edition is detected by size only, and only when the
/// stored row recorded one.
pub(crate) fn edition_drift(
    stored: &[AlternateMediaSource],
    disk: &[(PathBuf, Option<u64>)],
) -> Option<(ReprobeReason, PathBuf)> {
    if let Some(gone) = stored
        .iter()
        .find(|s| !disk.iter().any(|(p, _)| *p == s.path))
    {
        return Some((ReprobeReason::Removed, gone.path.clone()));
    }
    if let Some((new, _)) = disk
        .iter()
        .find(|(p, _)| !stored.iter().any(|s| s.path == *p))
    {
        return Some((ReprobeReason::Added, new.clone()));
    }
    stored.iter().find_map(|s| {
        let now = disk.iter().find(|(p, _)| *p == s.path)?.1?;
        (s.size_bytes? != now).then(|| (ReprobeReason::Changed, s.path.clone()))
    })
}

/// Look up what `edition_drift` needs for the primary held by `row`: the
/// stored editions and a stat of each grouped alternate. A vanished alternate
/// stats as `None` and surfaces as a path-set difference or is ignored for the
/// size comparison, never as an error.
async fn edition_drift_for<S: MediaStore>(
    store: &S,
    row: MediaId,
    row_has_editions: bool,
    alts: &[(String, PathBuf)],
) -> DomainResult<Option<(ReprobeReason, PathBuf)>> {
    if !row_has_editions && alts.is_empty() {
        return Ok(None);
    }
    let stored = store.alternate_sources(row).await?;
    if stored.is_empty() && alts.is_empty() {
        return Ok(None);
    }
    let mut disk = Vec::with_capacity(alts.len());
    for (_, path) in alts {
        let size = tokio::fs::metadata(path).await.ok().map(|m| m.len());
        disk.push((path.clone(), size));
    }
    Ok(edition_drift(&stored, &disk))
}

/// Make an edition-set change visible: one info line carrying the item, its
/// primary path, the reason and the offending edition path, plus the counter.
/// Query: `{namespace="pharos"} |= "edition set changed"` or
/// `sum by (reason) (increase(pharos_scan_reprobe_total[1h]))`.
fn record_edition_drift(
    row: MediaId,
    primary: &Path,
    reason: ReprobeReason,
    edition: &Path,
    outcome: &'static str,
) {
    tracing::info!(
        media.id = %row,
        path = %primary.display(),
        reason = reason.label(),
        edition = %edition.display(),
        outcome,
        "edition set changed since the stored row was written",
    );
    metrics::counter!("pharos_scan_reprobe_total", "reason" => reason.label()).increment(1);
}

/// B1023 — whether an unchanged file's stored row is a film the show folder
/// above it now makes an episode (V966's rule, which a schema bump would
/// otherwise have to re-probe the whole library to apply). Asked only of a
/// video that [`is_episode_path`] does not already call an episode, so the
/// row read happens only for the few files the rule changed.
async fn kind_drift<S: MediaStore>(store: &S, row: MediaId, primary: &Path) -> bool {
    if is_episode_path(primary) || !is_show_folder_video(primary) {
        return false;
    }
    let stored = match store.get(row).await {
        Ok(item) => item.kind,
        Err(err) => {
            tracing::warn!(media.id = %row, path = %primary.display(), error = %err, "could not read a row to check its kind");
            return false;
        }
    };
    let drifted = stored != MediaKind::Episode;
    if drifted {
        tracing::info!(
            media.id = %row,
            path = %primary.display(),
            stored = stored.as_str(),
            "a film row sits under a show folder; re-probing it as an episode",
        );
        metrics::counter!("pharos_scan_reprobe_total", "reason" => "kind_changed").increment(1);
    }
    drifted
}

/// A file grouped this pass as a version of `into`, whose own row (if it
/// still has one) hands its viewers' state over before the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fold {
    copy: PathBuf,
    /// Each item, and whether the copy's resume point carries into it.
    into: Vec<(PathBuf, pharos_core::FoldResume)>,
}

/// Every file of `groups` that is a version of another and no item of its
/// own, with each item it is now a version of. A file that is still an item
/// (a double episode that is the primary of an episode no single covers)
/// keeps its row and its state, so it folds nowhere: folding it each pass
/// would re-mark an episode its viewer had marked unwatched. Resume carries
/// only into an item of the same episode span as the copy.
fn folded_copies(groups: &[(PathBuf, Vec<(String, PathBuf)>)]) -> Vec<Fold> {
    let items: HashSet<&Path> = groups.iter().map(|(p, _)| p.as_path()).collect();
    let mut folds: BTreeMap<&Path, Vec<&Path>> = BTreeMap::new();
    for (primary, alts) in groups {
        for (_, alt) in alts {
            if !items.contains(alt.as_path()) {
                folds.entry(alt).or_default().push(primary);
            }
        }
    }
    folds
        .into_iter()
        .map(|(copy, into)| {
            let span = |p: &Path| episode_copy_key(p).map(|(_, _, first, last)| (first, last));
            let own = span(copy);
            Fold {
                copy: copy.to_path_buf(),
                into: into
                    .into_iter()
                    .map(|p| {
                        let resume = if span(p) == own {
                            pharos_core::FoldResume::Carry
                        } else {
                            pharos_core::FoldResume::Drop
                        };
                        (p.to_path_buf(), resume)
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Settle each file now grouped as a version of another before the sweep.
/// When every item it is now a version of was written this pass, the copy's
/// old row hands each viewer's played, resume and favourite state to each of
/// them (idempotent: the store keeps the furthest state) and is then swept.
/// When one was not (its probe failed, or it is known bad), the copy keeps
/// its own row, so a playable copy never vanishes behind an unplayable
/// primary.
async fn settle_copies<S: MediaStore>(
    store: &S,
    scan_id: i64,
    folded: &[Fold],
) -> DomainResult<()> {
    for fold in folded {
        let Some(from) = store.id_at_path(&fold.copy).await? else {
            continue;
        };
        let mut written = Vec::with_capacity(fold.into.len());
        let mut unwritten = None;
        for (primary, resume) in &fold.into {
            let into = match store.id_at_path(primary).await? {
                Some(into) => store
                    .scan_state(into)
                    .await?
                    .filter(|s| s.last_seen_scan_id == scan_id)
                    .map(|_| into),
                None => None,
            };
            match into {
                Some(into) => written.push((into, *resume)),
                None => unwritten = Some(primary),
            }
        }
        if let Some(primary) = unwritten {
            let Ok(meta) = tokio::fs::metadata(&fold.copy).await else {
                continue;
            };
            store
                .mark_seen(from, scan_id, mtime_secs(&meta), meta.len())
                .await?;
            tracing::warn!(
                row = %from,
                version = %fold.copy.display(),
                primary = %primary.display(),
                "kept a copy as its own item: the file it groups under was not written this pass",
            );
            continue;
        }
        for (into, resume) in written.into_iter().filter(|(into, _)| *into != from) {
            store.merge_user_data(from, into, resume).await?;
            tracing::info!(
                from = %from,
                into = %into,
                version = %fold.copy.display(),
                resume = ?resume,
                "folded a copy's row into the item it is now a version of",
            );
        }
    }
    Ok(())
}

/// P41 — group walk output into `(primary, Vec<(edition_label, alt_path)>)`
/// tuples. Files whose stem matches `Title - <known_edition>` and that
/// share a directory + a primary file (`Title.ext`) are demoted to
/// alternates of the primary. Files without a matching primary
/// remain stand-alone items (the edition tag is preserved in the
/// title).
#[cfg(test)]
pub(crate) fn group_editions(paths: Vec<PathBuf>) -> Vec<(PathBuf, Vec<(String, PathBuf)>)> {
    group_copies(
        group_by_edition(paths),
        &HashMap::new(),
        &mut CopyTally::default(),
    )
}

/// P41's half of [`group_editions`]: editions under their primaries, before
/// copies of one episode are folded together.
fn group_by_edition(paths: Vec<PathBuf>) -> Vec<(PathBuf, Vec<(String, PathBuf)>)> {
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

/// The files of `groups` that share an episode with another file: only these
/// are ranked, so only these are stat'ed. Each is flagged when some copy of
/// one of its episodes states no height in its name: only then is the store
/// asked for stored heights, so a library named `WEBDL-1080p` style costs no
/// reads.
fn copy_candidates(groups: &[(PathBuf, Vec<(String, PathBuf)>)]) -> Vec<(PathBuf, bool)> {
    let mut by_bucket: BTreeMap<CopyBucket, Vec<&Path>> = BTreeMap::new();
    let buckets = library_buckets(groups.iter().map(|g| g.0.as_path()));
    for ((primary, _), buckets) in groups.iter().zip(buckets) {
        for bucket in buckets {
            by_bucket.entry(bucket).or_default().push(primary);
        }
    }
    let named = |p: &Path| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .and_then(name_height)
            .is_some()
    };
    let mut out: BTreeMap<PathBuf, bool> = BTreeMap::new();
    for (bucket, members) in by_bucket.into_iter().filter(|(_, m)| m.len() > 1) {
        let unnamed = bucket.has_picture() && !members.iter().all(|p| named(p));
        for path in members {
            let flag = out.entry(path.to_path_buf()).or_default();
            *flag |= unnamed;
        }
    }
    out.into_iter().collect()
}

/// Each candidate's size on disk; a file that cannot be stat'ed ranks as if
/// empty.
async fn copy_sizes(candidates: &[PathBuf]) -> HashMap<PathBuf, CopyFacts> {
    let mut out = HashMap::with_capacity(candidates.len());
    for path in candidates {
        let size = tokio::fs::metadata(path).await.ok().map(|m| m.len());
        out.insert(path.clone(), CopyFacts { height: None, size });
    }
    out
}

/// [`copy_sizes`] plus the height each flagged candidate was last probed at:
/// from its own row, or from the row it was folded into as a version. A store
/// fault leaves the heights unknown (ranking falls back to the file names)
/// rather than failing the scan.
async fn copy_facts<S: MediaStore>(
    store: &S,
    candidates: &[(PathBuf, bool)],
) -> HashMap<PathBuf, CopyFacts> {
    let paths: Vec<PathBuf> = candidates.iter().map(|(p, _)| p.clone()).collect();
    let mut out = copy_sizes(&paths).await;
    for (path, _) in candidates.iter().filter(|(_, look_up)| *look_up) {
        let row = match store.id_at_path(path).await {
            Ok(Some(row)) => row,
            Ok(None) => continue,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "could not look up a copy's row to rank it");
                continue;
            }
        };
        let item = match store.get(row).await {
            Ok(item) => item,
            Err(err) => {
                tracing::warn!(path = %path.display(), row = %row, error = %err, "could not read a copy's row to rank it");
                continue;
            }
        };
        let mut heights = vec![(
            item.path.clone(),
            picture_lines(item.probe.width, item.probe.height),
        )];
        heights.extend(item.probe.alternate_sources.iter().map(|alt| {
            let probe = alt.probe.as_ref();
            (
                alt.path.clone(),
                picture_lines(probe.and_then(|p| p.width), probe.and_then(|p| p.height)),
            )
        }));
        for (at, height) in heights {
            if let (Some(facts), Some(height)) = (out.get_mut(&at), height) {
                facts.height.get_or_insert(height);
            }
        }
    }
    out
}

/// A picture's resolution class in lines: its height, or the height a 16:9
/// frame of its width would have, whichever is more. A letterboxed 1920x800
/// Blu-ray rip is a 1080-line copy, not one below a 1440x1080 one.
fn picture_lines(width: Option<u32>, height: Option<u32>) -> Option<u32> {
    let from_width = width.map(|w| w.saturating_mul(9) / 16);
    match (height, from_width) {
        (Some(h), Some(w)) => Some(h.max(w)),
        (h, w) => h.or(w),
    }
}

/// What is known of a file's picture before this pass probes it: the height
/// a stored probe recorded, and its size on disk. Ranks copies of one episode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CopyFacts {
    pub(crate) height: Option<u32>,
    pub(crate) size: Option<u64>,
}

/// A span longer than this is a season pack or a misparse (`S01E01-E99`),
/// so it groups only with files of exactly its span, as before V844.
const MAX_EPISODE_SPAN: u32 = 8;

/// `(show folder, season, first episode, last episode)` of an episode file.
type EpisodeSpan = (String, u32, u32, u32);

/// One title that several files can be copies of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CopyBucket {
    /// `first` and `last` are one episode, or a long span's own ends.
    Episode {
        show: String,
        season: u32,
        first: u32,
        last: u32,
    },
    /// A film by its cleaned, case-folded title and its year.
    Film { title: String, year: u32 },
    /// A song by its folded file stem and the folders above it (album and
    /// artist, or disc, album and artist), release tags such as `[FLAC]` left
    /// out.
    Song { folders: Vec<String>, stem: String },
}

impl CopyBucket {
    /// Whether a stored probe's picture height can rank this title's copies:
    /// a song has no picture, so its copies are never looked up.
    fn has_picture(&self) -> bool {
        !matches!(self, Self::Song { .. })
    }
}

/// The containers a film is held in: an audio file or a book is never a
/// copy of a film.
const FILM_EXTENSIONS: &[&str] = &["mkv", "mp4", "mov", "avi", "webm", "m4v"];

/// The buckets `path` is a copy in: for an episode file each episode it
/// covers, or for a span past [`MAX_EPISODE_SPAN`] only its exact span; for a
/// film its title and year; for a song its name and folders.
fn copy_buckets(path: &Path) -> Vec<CopyBucket> {
    let Some((show, season, first, last)) = episode_copy_key(path) else {
        return film_copy_key(path)
            .map(|(title, year)| CopyBucket::Film { title, year })
            .or_else(|| song_copy_key(path))
            .into_iter()
            .collect();
    };
    if last.saturating_sub(first) >= MAX_EPISODE_SPAN {
        return vec![CopyBucket::Episode {
            show,
            season,
            first,
            last,
        }];
    }
    (first..=last)
        .map(|episode| CopyBucket::Episode {
            show: show.clone(),
            season,
            first: episode,
            last: episode,
        })
        .collect()
}

/// A primary file and its `(label, path)` versions.
type Group = (PathBuf, Vec<(String, PathBuf)>);

/// Fold copies of one title into a single group: files under the same show
/// folder that cover the same season and episode (`Season 01` and `Season 1`
/// both holding S01E02, a `.avi` beside its `.mkv` re-rip, or a double
/// `S01E01-E02` beside a single `S01E01`). A file spanning several episodes is
/// a copy of each one it covers. Each episode's best-ranked file is its
/// primary (see [`CopyRank`]); the others become its versions, labelled by
/// what tells them apart in their names. A file stays its own item while it is
/// the primary of any episode it covers, so a double with a single for E01 but
/// none for E02 is a version of E01 and the item for E02 (V721, V844).
fn group_copies(
    groups: Vec<(PathBuf, Vec<(String, PathBuf)>)>,
    facts: &HashMap<PathBuf, CopyFacts>,
    tally: &mut CopyTally,
) -> Vec<(PathBuf, Vec<(String, PathBuf)>)> {
    group_copies_guarded(groups, facts, &FoldGuard::default(), tally)
}

/// [`group_copies`], with episode copies bucketed by the series key the
/// split gives them and a copy that may be another show's kept as its own
/// item (V894).
fn group_copies_guarded(
    groups: Vec<(PathBuf, Vec<(String, PathBuf)>)>,
    facts: &HashMap<PathBuf, CopyFacts>,
    guard: &FoldGuard,
    tally: &mut CopyTally,
) -> Vec<(PathBuf, Vec<(String, PathBuf)>)> {
    let spans: Vec<Option<EpisodeSpan>> = groups.iter().map(|g| episode_copy_key(&g.0)).collect();
    let mut covered = vec![false; groups.len()];
    let mut by_bucket: BTreeMap<CopyBucket, Vec<usize>> = BTreeMap::new();
    let buckets = library_buckets(groups.iter().map(|g| g.0.as_path()));
    for (i, buckets) in buckets.into_iter().enumerate() {
        let Some((path, _)) = groups.get(i) else {
            continue;
        };
        for bucket in buckets {
            let bucket = guard.rekey(path, bucket);
            by_bucket.entry(bucket).or_default().push(i);
            if let Some(flag) = covered.get_mut(i) {
                *flag = true;
            }
        }
    }
    // Each bucket's primary, and the copies it gathers in bucket order.
    let mut is_item = vec![false; groups.len()];
    let mut gathered: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (bucket, members) in &by_bucket {
        let rank = |i: &usize| {
            let span = spans.get(*i).and_then(Option::as_ref);
            let path = groups.get(*i).map(|g| g.0.as_path());
            (CopyRank::of(path, span, facts), *i)
        };
        if let [only] = members.as_slice() {
            if let Some(flag) = is_item.get_mut(*only) {
                *flag = true;
            }
            continue;
        }
        let Some(&primary) = members.iter().min_by_key(|i| rank(i)) else {
            continue;
        };
        if let Some(flag) = is_item.get_mut(primary) {
            *flag = true;
        }
        let ranked: Vec<_> = members.iter().map(rank).collect();
        let decided_by = CopyRank::decided_by(&ranked);
        tally.record(decided_by);
        tracing::debug!(
            bucket = ?bucket,
            primary = %groups.get(primary).map_or_else(String::new, |g| g.0.display().to_string()),
            decided_by = decided_by.label(),
            candidates = ?ranked.iter().map(|(r, i)| (groups.get(*i).map(|g| g.0.display().to_string()), r)).collect::<Vec<_>>(),
            "chose the primary of a title's copies",
        );
        let primary_path = groups.get(primary).map(|g| g.0.as_path());
        let mut kept = Vec::with_capacity(members.len());
        for &member in members {
            if member == primary {
                continue;
            }
            let refusal = match (guard.mode, bucket, primary_path, groups.get(member)) {
                (Some(mode), CopyBucket::Episode { .. }, Some(a), Some((b, _))) => {
                    guard.refusal(a, b).map(|reason| (mode, reason))
                }
                _ => None,
            };
            let Some((mode, reason)) = refusal else {
                kept.push(member);
                continue;
            };
            tally.record(CopyDecision::RefusedOtherSeries);
            let primary_shown = primary_path.map_or_else(String::new, |p| p.display().to_string());
            let copy_shown = groups
                .get(member)
                .map_or_else(String::new, |g| g.0.display().to_string());
            // An admin's own placement holds in a dry run too: the scanner
            // did not decide it, and folding it would merge user data (V896).
            if mode == crate::series_split::RepairMode::Apply || reason == "admin_placed" {
                if let Some(flag) = is_item.get_mut(member) {
                    *flag = true;
                }
                tracing::info!(
                    primary = %primary_shown,
                    copy = %copy_shown,
                    reason,
                    mode = mode.label(),
                    "refused to fold a copy that may be another show's episode",
                );
            } else {
                tracing::info!(
                    primary = %primary_shown,
                    copy = %copy_shown,
                    reason,
                    mode = mode.label(),
                    "would refuse to fold a copy that may be another show's episode; folded as before",
                );
                kept.push(member);
            }
        }
        let entry = gathered.entry(primary).or_default();
        for member in kept {
            if !entry.contains(&member) {
                entry.push(member);
            }
        }
    }
    let mut out = Vec::with_capacity(groups.len());
    let mut taken: Vec<Option<Group>> = groups.into_iter().map(Some).collect();
    let paths: Vec<PathBuf> = taken
        .iter()
        .map(|g| g.as_ref().map(|g| g.0.clone()).unwrap_or_default())
        .collect();
    for i in 0..taken.len() {
        let covered = covered.get(i).copied().unwrap_or(false);
        if covered && !is_item.get(i).copied().unwrap_or(false) {
            continue;
        }
        let Some((primary, mut alts)) = taken.get_mut(i).and_then(Option::take) else {
            continue;
        };
        for &copy in gathered.get(&i).map(Vec::as_slice).unwrap_or_default() {
            let Some(path) = paths.get(copy) else {
                continue;
            };
            let label = copy_label(&primary, path);
            tracing::debug!(
                primary = %primary.display(),
                copy = %path.display(),
                label = %label,
                "grouped a copy of a title as a version",
            );
            alts.push((label, path.clone()));
            // A copy that is no item of its own brings its editions along,
            // to the first item it folds into.
            if !is_item.get(copy).copied().unwrap_or(false) {
                if let Some((_, editions)) = taken.get_mut(copy).and_then(Option::take) {
                    alts.extend(editions);
                }
            }
        }
        out.push((primary, alts));
    }
    out
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What the series split (V893) and the stored matches say about each
/// episode file, for the copy fold (V894). The default guards nothing.
#[derive(Debug, Default)]
pub(crate) struct FoldGuard {
    /// `None`: no checks. `DryRun`: log and count each copy that would be
    /// kept apart, and fold it as before. `Apply`: keep it apart.
    mode: Option<crate::series_split::RepairMode>,
    /// Each file's series key, where the split plan assigned one.
    keys: HashMap<PathBuf, String>,
    /// Each file's stated show year (V893), where it states one.
    stated: HashMap<PathBuf, u32>,
    /// Files an `admin` per-path override placed (V896).
    admin_placed: std::collections::HashSet<PathBuf>,
    /// Series keys whose copies fold only on agreeing episode titles: a
    /// split folder's keys and keys whose show match is flagged (V891).
    guarded: std::collections::HashSet<String>,
}

/// Two filename episode titles at or above this similarity name one episode
/// (spec 033 §5, the same bar V891 scores titles by).
const SAME_EPISODE_TITLE: f32 = 0.8;

impl FoldGuard {
    pub(crate) fn new(
        folders: &BTreeMap<String, Vec<(PathBuf, Option<u32>)>>,
        plan: &crate::series_split::SeriesPlan,
        ctx: &crate::series_split::PlanContext,
        ambiguous: std::collections::HashSet<String>,
        mode: crate::series_split::RepairMode,
    ) -> Self {
        let keys: HashMap<PathBuf, String> = plan
            .assignments
            .iter()
            .map(|(p, a)| (p.clone(), a.series_key.clone()))
            .collect();
        let stated = folders
            .values()
            .flatten()
            .filter_map(|(p, y)| Some((p.clone(), (*y)?)))
            .collect();
        let admin_placed = ctx
            .overrides
            .iter()
            .filter(|o| o.source == pharos_core::OverrideSource::Admin)
            .filter_map(|o| o.path.as_deref().map(PathBuf::from))
            .collect();
        let mut guarded = ambiguous;
        for split in &plan.splits {
            guarded.extend(
                keys.values()
                    .filter(|k| {
                        k.as_str() == split.folder
                            || k.strip_prefix(split.folder.as_str()).is_some_and(|rest| {
                                rest.starts_with(crate::series_split::SPLIT_KEY_SEPARATOR)
                            })
                    })
                    .cloned(),
            );
            guarded.insert(split.folder.clone());
        }
        Self {
            mode: Some(mode),
            keys,
            stated,
            admin_placed,
            guarded,
        }
    }

    /// The keys whose show match may be flagged: every folder walked and
    /// every key the plan assigned.
    pub(crate) fn match_keys(
        folders: &BTreeMap<String, Vec<(PathBuf, Option<u32>)>>,
        plan: &crate::series_split::SeriesPlan,
    ) -> Vec<String> {
        let mut keys: std::collections::BTreeSet<String> = folders.keys().cloned().collect();
        keys.extend(plan.assignments.values().map(|a| a.series_key.clone()));
        keys.into_iter().collect()
    }

    /// `bucket` with an episode's show folder replaced by its series key.
    fn rekey(&self, path: &Path, bucket: CopyBucket) -> CopyBucket {
        match (bucket, self.keys.get(path)) {
            (
                CopyBucket::Episode {
                    season,
                    first,
                    last,
                    ..
                },
                Some(key),
            ) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let show = match name_show_year(name) {
                    Some(year) => format!("{key}\u{1f}{year}"),
                    None => key.clone(),
                };
                CopyBucket::Episode {
                    show,
                    season,
                    first,
                    last,
                }
            }
            (bucket, _) => bucket,
        }
    }

    fn key_of(&self, path: &Path) -> Option<String> {
        self.keys
            .get(path)
            .cloned()
            .or_else(|| episode_series_info(path)?.series_folder)
    }

    /// Why `copy` must not fold into `primary`, or `None` when it may.
    fn refusal(&self, primary: &Path, copy: &Path) -> Option<&'static str> {
        if let (Some(a), Some(b)) = (self.stated.get(primary), self.stated.get(copy)) {
            if a != b {
                return Some("years_differ");
            }
        }
        if self.admin_placed.contains(primary) || self.admin_placed.contains(copy) {
            return Some("admin_placed");
        }
        // An edition tag (` - 3D.HSBS`, ` - Extended`) names the copy, not
        // the episode.
        let title = |p: &Path| {
            let stem = p.file_stem()?.to_str()?;
            let stem = split_edition_tag(stem).map_or(stem, |(left, _)| left);
            crate::metadata::filename::episode_title(stem)
        };
        let same_span = episode_copy_key(primary).map(|k| (k.2, k.3))
            == episode_copy_key(copy).map(|k| (k.2, k.3));
        let titles = same_span.then(|| (title(primary), title(copy)));
        match titles {
            Some((Some(a), Some(b))) => {
                if pharos_core::title_similarity(&a, &b) < SAME_EPISODE_TITLE {
                    Some("titles_differ")
                } else {
                    None
                }
            }
            _ => [primary, copy]
                .iter()
                .any(|p| self.key_of(p).is_some_and(|k| self.guarded.contains(&k)))
                .then_some("numbers_only"),
        }
    }
}

/// The title a film file gives when neither its name nor its folder's states
/// a year (`The Secret Life of Pets/The Secret Life of Pets.avi`). Such a
/// file joins a film of that title only when the library holds the title in
/// a single year (see [`library_buckets`]).
fn yearless_film_title(path: &Path) -> Option<String> {
    if is_episode_file(path) {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !FILM_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let year_of = |name: &str| crate::metadata::filename::parse_filename(name, true).year;
    let stem = path.file_stem()?.to_str()?;
    let folder = path.parent()?.file_name()?.to_str()?;
    if year_of(stem).is_some() || year_of(folder).is_some() {
        return None;
    }
    let title = yearless_title(stem);
    (!title.is_empty()).then(|| title.join(" "))
}

/// Each path's [`copy_buckets`], plus, for a film named with no year in a
/// folder with none, the one year the rest of the library holds its title in.
/// A title held in two years (a remake) leaves such a file apart, since
/// nothing says which film it is.
fn library_buckets<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<Vec<CopyBucket>> {
    let paths: Vec<&Path> = paths.collect();
    let mut out: Vec<Vec<CopyBucket>> = paths.iter().map(|p| copy_buckets(p)).collect();
    let mut years: BTreeMap<String, std::collections::BTreeSet<u32>> = BTreeMap::new();
    for bucket in out.iter().flatten() {
        if let CopyBucket::Film { title, year } = bucket {
            years.entry(title.clone()).or_default().insert(*year);
        }
    }
    for (path, buckets) in paths.iter().zip(out.iter_mut()) {
        if !buckets.is_empty() {
            continue;
        }
        let Some(title) = yearless_film_title(path) else {
            continue;
        };
        let held: Vec<u32> = years
            .get(&title)
            .map(|y| y.iter().copied().collect())
            .unwrap_or_default();
        if let [year] = held.as_slice() {
            tracing::debug!(path = %path.display(), title, year, "a yearless film joined the one year its title is held in");
            buckets.push(CopyBucket::Film { title, year: *year });
        } else if held.len() > 1 {
            tracing::debug!(path = %path.display(), title, years = ?held, "a yearless film kept apart: its title is held in several years");
        }
    }
    out
}

/// The key a film file's name gives it (`Heat (1995) Bluray-1080p.mkv` is
/// `("heat", 1995)`): the name's words, lower-cased, less its year, the words
/// that say how the copy was made ([`FILM_RELEASE_WORDS`], resolutions,
/// channel layouts, the 3D packing) and a leading or trailing article
/// (`The Thing`, `Thing, The`). A cut or edition after the year and a lone
/// `cd1` are dropped too, so each is a version of the film (V962, V963).
/// Whatever else the name says stays in the key, so a trailer, a sample, a
/// part or an episode of a show named by number is its own title. A name with
/// no year takes its folder's year only
/// when it names the folder's title (`Thing, The (1982)/The Thing.mkv`), since
/// a film's folder also holds its extras; with neither, it is no copy of
/// anything, since a title alone would merge remakes.
fn film_copy_key(path: &Path) -> Option<(String, u32)> {
    if is_episode_file(path) {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !FILM_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let year_of = |name: &str| crate::metadata::filename::parse_filename(name, true).year;
    let (title, year) = match year_of(stem) {
        Some(year) => (film_title(stem, year), year),
        None => {
            let folder = path.parent()?.file_name()?.to_str()?;
            let year = year_of(folder)?;
            let title = film_title(folder, year);
            let named = film_title(stem, year) == title;
            let trimmed = || film_title(&strip_trailing_cut(stem), year) == title;
            (named || trimmed()).then_some((title, year))?
        }
    };
    (!title.is_empty()).then(|| (title.join(" "), year))
}

/// A release group at the end of a name: hyphenated and bracketed
/// (`-[RARBG]`), a bracketed domain (`[YTS.AG]`), a scene `-GROUP` straight
/// after an encode word (`x264-SPARKS`, `x264-MgB [ETRG]`), or a `.GROUP`
/// straight after a codec (`x264.Deceit`). A plain trailing bracket
/// (`[Extended]`) is an edition and stays.
static RELEASE_GROUP_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(
    || {
        regex::Regex::new(
            r"(?i)(?:-\s*\[[^\[\]]*\]\s*$|[-. ]*\[[^\[\]]*\.[^\[\]]*\]\s*$|(?P<enc>x26[45]|h\.?26[45]|hevc|xvid|divx|av1|\d{3,4}p|bluray|web-?dl|webrip|hdtv|aac|ac3|dts|atmos)-[a-z0-9.]+(?:\s*\[[^\[\]]*\])?$|(?P<codec>x26[45]|h\.?26[45]|hevc|xvid|divx)\.[a-z0-9]+$)",
        )
        .ok()
    },
);

/// `name` less a trailing release group (see [`RELEASE_GROUP_RE`]), repeated
/// while one is left (`x264-[YTS.AG]`).
fn strip_release_group(name: &str) -> String {
    let mut name = name.trim().to_string();
    let Some(re) = RELEASE_GROUP_RE.as_ref() else {
        return name;
    };
    while let Some(caps) = re.captures(&name) {
        let (Some(all), enc) = (caps.get(0), caps.name("enc")) else {
            break;
        };
        let cut = enc
            .or_else(|| caps.name("codec"))
            .map_or(all.start(), |e| e.end());
        let next = name.get(..cut).unwrap_or("").trim_end().to_string();
        if next.len() == name.len() {
            break;
        }
        name = next;
    }
    name
}

/// A film name's title words: [`film_copy_key`]'s words less the year, the
/// release words and an article at either end.
fn film_title(name: &str, year: u32) -> Vec<String> {
    title_words(name, Some(year))
}

/// [`film_title`] for a name with no year, which keeps any cut it states.
fn yearless_title(name: &str) -> Vec<String> {
    title_words(name, None)
}

fn title_words(name: &str, year: Option<u32>) -> Vec<String> {
    // `_` is a word character to the cut and group patterns: a scene name
    // spelt with underscores would keep both.
    let name = strip_release_group(&name.replace('_', " "));
    let name = match CHANNELS_RE.as_ref() {
        Some(re) => re.replace_all(&name, " "),
        None => name.into(),
    };
    let year = year.map(|y| y.to_string()).unwrap_or_default();
    let name = if year.is_empty() {
        name
    } else {
        strip_cut(&name, &year).into_owned().into()
    };
    let mut all = words(&name);
    if let Some(at) = all.iter().rposition(|w| !year.is_empty() && *w == year) {
        let tail = all.split_off(at + 1);
        if tail.iter().any(|w| is_film_release_word(w)) {
            all.extend(disc_words(&tail));
        } else {
            all.extend(tail);
        }
    }
    let mut title: Vec<String> = all
        .into_iter()
        .filter(|w| *w != year && !is_film_release_word(w))
        .collect();
    let article = |w: &String| matches!(w.as_str(), "the" | "a" | "an");
    if title.len() > 1 && title.first().is_some_and(article) {
        title.remove(0);
    }
    if title.len() > 1 && title.last().is_some_and(article) {
        title.pop();
    }
    // `The Hobbit The Battle…` and `The.Hobbit.Battle…` name one film.
    if title.len() > 1 {
        title.retain(|w| w != "the");
    }
    title
}

/// Words that make a file a film's extra, not a copy of it: a scene tail
/// keeps them (`Heat.1995.Trailer.720p` is no copy of `Heat (1995)`).
const EXTRA_WORDS: &[&str] = &[
    "trailer",
    "teaser",
    "featurette",
    "featurettes",
    "behind",
    "scenes",
    "deleted",
    "interview",
    "interviews",
    "extra",
    "extras",
    "bonus",
    "making",
    "short",
    "clip",
    "preview",
    "outtakes",
    "bloopers",
    "gag",
    "reel",
];

/// The disc or part a scene tail names (`cd2`, `CD 2`, `Part 2`): the one
/// thing after the year in a release name that tells two files' contents
/// apart. The rest of such a tail (`3Li BluRay 720p`, `Eng NL Subs`,
/// `DTS-HD MA-LTT`) says only how the copy was made.
fn disc_words(tail: &[String]) -> Vec<String> {
    let numbered = |w: &str| w.bytes().all(|b| b.is_ascii_digit()) && !w.is_empty();
    let marker = |w: &str| matches!(w, "cd" | "disc" | "disk" | "part" | "pt");
    let mut out = Vec::new();
    for (i, w) in tail.iter().enumerate() {
        let joined = ["cd", "disc", "disk", "part", "pt"]
            .iter()
            .any(|m| w.strip_prefix(m).is_some_and(numbered));
        if joined || EXTRA_WORDS.contains(&w.as_str()) {
            out.push(w.clone());
        } else if marker(w) && tail.get(i + 1).is_some_and(|n| numbered(n)) {
            out.extend(tail.get(i..=i + 1).into_iter().flatten().cloned());
        }
    }
    out
}

/// `name` less the cut its tail names after `year` (`Director's Cut`,
/// `{edition-Extended}`, `IMAX`, a lone `cd1`), so each cut of a film is a
/// version of it. A cut word before the year is part of the title.
fn strip_cut<'a>(name: &'a str, year: &str) -> std::borrow::Cow<'a, str> {
    let Some(at) = name.rfind(year) else {
        return name.into();
    };
    let (head, tail) = name.split_at(at);
    match CUT_RE.as_ref() {
        Some(re) if re.is_match(tail) => format!("{head}{}", re.replace_all(tail, " ")).into(),
        _ => name.into(),
    }
}

/// `name` less a cut that ends it (`The Bourne Identity cd1`), for a name with
/// no year to compare with its folder's title.
fn strip_trailing_cut(name: &str) -> String {
    let name = name.trim_end();
    let end = CUT_RE.as_ref().and_then(|re| {
        re.find_iter(name)
            .filter(|m| {
                name.get(m.end()..).is_some_and(|rest| {
                    rest.trim_matches(|c: char| c.is_whitespace() || ")]}".contains(c))
                        .is_empty()
                })
            })
            .map(|m| m.start())
            .next()
    });
    end.and_then(|at| name.get(..at))
        .unwrap_or(name)
        .to_string()
}

/// A cut or edition a film's name may state after its year, or a `cd1` that
/// holds the whole film.
static CUT_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\{edition-[^{}]*\}|\b(?:director['’]?s|extended|theatrical|ultimate|final|collector['’]?s|special|anniversary|unrated|uncut|uncensored|remastered|imax|criterion|redux)(?:[ ._-]+(?:cut|edition|version|extended|uncut))*\b(?:[ ._-]+(?:cut|edition|version))?|\bcd ?0?1\b",
    )
    .ok()
});

/// Words of a film's name that say how a copy was made, not what it holds.
const FILM_RELEASE_WORDS: &[&str] = &[
    "4k",
    "8k",
    "uhd",
    "fhd",
    "hd",
    "sd",
    "bluray",
    "blu",
    "ray",
    "bdrip",
    "brrip",
    "bdremux",
    "remux",
    "bd",
    "web",
    "dl",
    "webdl",
    "webrip",
    "hdtv",
    "pdtv",
    "dvdrip",
    "dvd",
    "hdrip",
    "tvrip",
    "x264",
    "x265",
    "h264",
    "h265",
    "h",
    "264",
    "265",
    "hevc",
    "avc",
    "xvid",
    "divx",
    "av1",
    "vp9",
    "8bit",
    "10bit",
    "12bit",
    "hdr",
    "hdr10",
    "hdr10plus",
    "dv",
    "dovi",
    "sdr",
    "aac",
    "ac3",
    "eac3",
    "dts",
    "ma",
    "truehd",
    "atmos",
    "ddp",
    "dd",
    "flac",
    "opus",
    "mp3",
    "proper",
    "repack",
    "yify",
    "yts",
    "rarbg",
    "3d",
    "sbs",
    "hsbs",
    "fsbs",
    "tab",
    "htab",
    "ftab",
    "ou",
    "hou",
    "fou",
    "mvc",
    "half",
    "full",
];

fn is_film_release_word(word: &str) -> bool {
    FILM_RELEASE_WORDS.contains(&word) || RESOLUTION_RE.as_ref().is_some_and(|re| re.is_match(word))
}

/// `1080p`, `2160p`, `1080i`, with a frame rate (`1080p60`).
static RESOLUTION_RE: std::sync::LazyLock<Option<regex::Regex>> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^\d{3,4}[pi]\d{0,3}$").ok());

/// An audio channel layout (`5.1`, `DDP5.1`, `AAC2.0`), taken out before the
/// name is split into words, so its digits never read as a part number.
static CHANNELS_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(?:ddp?|aac|e?ac3|dts|truehd|atmos|opus|flac)?\d\.\d\b").ok()
});

/// `name`'s words: runs of letters and digits, lower-cased.
fn words(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The containers a song is held in.
const SONG_EXTENSIONS: &[&str] = &["flac", "mp3", "opus", "m4a", "ogg", "wav"];

/// Words a bracketed tag in a music name may hold for the tag to say only how
/// the copy was ripped: formats, bit depths, sample and bit rates.
const SONG_FORMAT_WORDS: &[&str] = &[
    "flac", "alac", "mp3", "aac", "m4a", "ogg", "vorbis", "opus", "wav", "lossless", "hi", "res",
    "hires", "web", "vbr", "cbr", "kbps", "khz", "bit", "16bit", "24bit", "32bit", "v0", "v2",
    "128k", "192k", "256k", "320k", "128", "160", "192", "256", "320", "16", "24", "32", "44",
    "48", "88", "96", "176", "1", "2",
];

/// A bracketed group, `[..]`, `(..)` or `{..}`, with nothing bracketed inside.
static BRACKET_RE: std::sync::LazyLock<Option<regex::Regex>> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"[\[({]([^\[\](){}]*)[\])}]").ok());

/// `name` folded as [`fold_name`] does, after dropping each bracketed tag
/// that holds only a year or format words (`[FLAC]`, `(2001)`,
/// `(320 kbps)`, `[24-96]`). Any other bracket stays, as does a lone number
/// (`Interlude (2)`), so `Album [CD2]`,
/// `Album (Deluxe)`, `Song [Live]` and `Song (Remix v2)` are their own.
fn fold_release_name(name: &str) -> String {
    let Some(re) = BRACKET_RE.as_ref() else {
        return fold_name(name);
    };
    let kept = re.replace_all(name, |caps: &regex::Captures<'_>| {
        let inner = caps.get(1).map_or("", |m| m.as_str());
        let inner = words(inner);
        let is_year = |w: &String| {
            w.len() == 4 && (w.starts_with("19") || w.starts_with("20")) && w.parse::<u32>().is_ok()
        };
        // A lone number is a track's own `(1)` or `(2)`, not a rate or depth.
        let lone_number = matches!(
            inner.as_slice(),
            [w] if w.chars().all(|c| c.is_ascii_digit()) && !is_year(w)
        );
        let tag = !lone_number
            && inner
                .iter()
                .all(|w| SONG_FORMAT_WORDS.contains(&w.as_str()) || is_year(w));
        if tag {
            " ".to_string()
        } else {
            caps.get(0)
                .map_or_else(String::new, |m| m.as_str().to_string())
        }
    });
    fold_name(&kept)
}

/// How many folders above a song name it with the file: album and artist,
/// or with a disc folder, disc, album and artist.
const SONG_FOLDERS: usize = 3;

/// The bucket of a song file: its folded stem with the folded names of the
/// [`SONG_FOLDERS`] folders above it. Copies of a song sit beside each other
/// (`01 Song.flac`, `01 Song.mp3`) or in a second rip of the album
/// (`Album [FLAC]/`, `Album (320 kbps)/`); the file name alone would merge
/// every artist's `01 Intro`. Tags are not read: they are only known once a
/// file is probed, which runs after this.
fn song_copy_key(path: &Path) -> Option<CopyBucket> {
    if is_episode_path(path) {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !SONG_EXTENSIONS.contains(&ext.as_str()) || is_theme_sidecar(path) {
        return None;
    }
    let stem = fold_release_name(path.file_stem()?.to_str()?);
    let folders: Vec<String> = path
        .ancestors()
        .skip(1)
        .take(SONG_FOLDERS)
        .map(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(fold_release_name)
                .unwrap_or_default()
        })
        .collect();
    folders.first().filter(|album| !album.is_empty())?;
    (!stem.is_empty()).then_some(CopyBucket::Song { folders, stem })
}

/// A show's or film's theme song (`theme.mp3`, or a file under
/// `theme-music/`): it belongs to the title folder it sits in, and two
/// shows of one name (`Show B`, `Show B (1996)`) are not copies.
fn is_theme_sidecar(path: &Path) -> bool {
    let named = |p: &Path, n: &str| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(n))
    };
    named(path, "theme")
        || path.ancestors().skip(1).any(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("theme-music"))
        })
}

/// `(show folder, season, first episode, last episode)` for an episode file
/// whose name carries both numbers; `None` keeps the file out of copy grouping.
fn episode_copy_key(path: &Path) -> Option<EpisodeSpan> {
    if !is_episode_file(path) {
        return None;
    }
    let info = episode_series_info(path)?;
    let first = info.episode_number?;
    let name = path.file_name()?.to_str()?;
    let last = episode_span_end(name)
        .filter(|l| *l > first)
        .unwrap_or(first);
    // Two shows of one name can share a folder (`Show A (1961) - S01E01`,
    // `Show A (1996) - S01E01`): a year the name states before its episode
    // number keeps them apart.
    let show = match name_show_year(name) {
        Some(year) => format!("{}\u{1f}{year}", info.series_folder?),
        None => info.series_folder?,
    };
    Some((show, info.season_number?, first, last))
}

static SHOW_YEAR_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?:^|[^0-9])((?:19|20)\d{2})(?:[^0-9]|$)").ok()
});

static EPISODE_TOKEN_RE: std::sync::LazyLock<Option<regex::Regex>> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\bs\d{1,4}e\d{1,4}|\b\d{1,2}x\d{2,3}\b").ok()
    });

/// The year an episode's file name states for its show, before the episode
/// number (`Show C (1988) - S01E01`); a year after it belongs to
/// the episode's title.
fn name_show_year(name: &str) -> Option<u32> {
    let token = EPISODE_TOKEN_RE.as_ref()?.find(name)?;
    let head = name.get(..token.start())?;
    SHOW_YEAR_RE
        .as_ref()?
        .captures_iter(head)
        .filter_map(|c| c.get(1)?.as_str().parse().ok())
        .last()
}

/// Spec 033 (V890) — the show name and year an episode's file name states
/// before its episode number (`Show C (1987) - S01E01` gives
/// `("Show C", 1987)`). The name is what precedes the year, so a
/// caller can check that the year is this show's.
pub(crate) fn name_show_name_and_year(name: &str) -> Option<(&str, u32)> {
    let token = EPISODE_TOKEN_RE.as_ref()?.find(name)?;
    let head = name.get(..token.start())?;
    let year = SHOW_YEAR_RE.as_ref()?.captures_iter(head).last()?.get(1)?;
    Some((head.get(..year.start())?, year.as_str().parse().ok()?))
}

static EP_SPAN_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)s\d{1,4}e\d{1,4}(?:[-_ ]?e(\d{1,4})|-(\d{1,3})\b)+").ok()
});

/// The last episode of a multi-episode name (`S01E01-E02`, `S01E01E02`,
/// `S01E01-02`); a bare `-NN` takes at most three digits and a word end, so
/// `S01E01-1080p` is no span.
fn episode_span_end(name: &str) -> Option<u32> {
    let caps = EP_SPAN_RE.as_ref()?.captures(name)?;
    [caps.get(1), caps.get(2)]
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().parse().ok())
        .max()
}

/// How one copy of an episode ranks for its primary, best first: a 2D file
/// before a 3D one (so a 3D copy never becomes the primary over a 2D copy), a
/// file of just that episode before one spanning several (it starts where the
/// episode does), then the taller picture, then the bigger file (copies of one
/// episode run the same length, so size stands for bitrate), then containers
/// that direct-play broadly before legacy ones, then the path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CopyRank {
    is_3d: bool,
    spans_more: bool,
    height: std::cmp::Reverse<u32>,
    size: std::cmp::Reverse<u64>,
    container: u8,
    path: PathBuf,
}

/// Which rule chose an episode's primary: the `decided_by` values of
/// `pharos_scan_copy_primaries`, a dashboard contract: do not rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CopyDecision {
    TwoD,
    SingleEpisode,
    Height,
    Size,
    Container,
    Path,
    /// A copy kept as its own item: it may be another show's episode (V894).
    RefusedOtherSeries,
}

impl CopyDecision {
    pub(crate) const ALL: [Self; 7] = [
        Self::TwoD,
        Self::SingleEpisode,
        Self::Height,
        Self::Size,
        Self::Container,
        Self::Path,
        Self::RefusedOtherSeries,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::TwoD => "2d",
            Self::SingleEpisode => "single_episode",
            Self::Height => "height",
            Self::Size => "size",
            Self::Container => "container",
            Self::Path => "path",
            Self::RefusedOtherSeries => "refused_other_series",
        }
    }
}

/// How many episodes with copies each rule decided in one scan pass.
#[derive(Debug, Default)]
pub(crate) struct CopyTally(HashMap<CopyDecision, u64>);

impl CopyTally {
    fn record(&mut self, decision: CopyDecision) {
        let n = self.0.entry(decision).or_default();
        *n = n.saturating_add(1);
    }

    /// Sets `pharos_scan_copy_primaries{decided_by}` to this pass's counts,
    /// every label included, so a gauge reads the library as last scanned
    /// whatever the scan rate.
    fn publish(&self) {
        for decision in CopyDecision::ALL {
            let n = self.0.get(&decision).copied().unwrap_or(0);
            metrics::gauge!("pharos_scan_copy_primaries", "decided_by" => decision.label())
                .set(n as f64);
        }
    }
}

impl CopyRank {
    /// `path`'s rank: its height from a stored probe, else its name
    /// (`1080p`, `4K`); a copy whose height is unknown ranks below one whose
    /// height is known.
    fn of(
        path: Option<&Path>,
        span: Option<&EpisodeSpan>,
        facts: &HashMap<PathBuf, CopyFacts>,
    ) -> Self {
        let path = path.unwrap_or_else(|| Path::new(""));
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        // A song has no picture: `01 Tab.mp3` is no 3D packing.
        let is_song = ext.as_deref().is_some_and(|e| SONG_EXTENSIONS.contains(&e));
        let is_3d = !is_song
            && (crate::metadata::filename::parse_video_3d_format(stem)
                .0
                .is_some()
                || split_edition_tag(stem)
                    .and_then(|(_, edition)| parse_3d_edition(edition))
                    .is_some());
        let container = match ext.as_deref() {
            Some("mkv") => 0,
            Some("mp4") => 1,
            Some("m4v") => 2,
            Some("webm") => 3,
            _ => 4,
        };
        let known = facts.get(path).copied().unwrap_or_default();
        Self {
            is_3d,
            spans_more: span.is_some_and(|(_, _, first, last)| last > first),
            height: std::cmp::Reverse(known.height.or_else(|| name_height(stem)).unwrap_or(0)),
            size: std::cmp::Reverse(known.size.unwrap_or(0)),
            container,
            path: path.to_path_buf(),
        }
    }

    /// The first rule on which the best of `ranked` beats the runner-up.
    fn decided_by<T>(ranked: &[(Self, T)]) -> CopyDecision {
        let mut sorted: Vec<&Self> = ranked.iter().map(|(r, _)| r).collect();
        sorted.sort();
        let (Some(best), Some(next)) = (sorted.first(), sorted.get(1)) else {
            return CopyDecision::Path;
        };
        if best.is_3d != next.is_3d {
            CopyDecision::TwoD
        } else if best.spans_more != next.spans_more {
            CopyDecision::SingleEpisode
        } else if best.height != next.height {
            CopyDecision::Height
        } else if best.size != next.size {
            CopyDecision::Size
        } else if best.container != next.container {
            CopyDecision::Container
        } else {
            CopyDecision::Path
        }
    }
}

static HEIGHT_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)(?:^|[^a-z0-9])(?:(4320|2160|1440|1080|720|576|480|360)[pi](?:\d{2,3})?|(4k|uhd|8k))(?:$|[^a-z0-9])").ok()
});

/// The picture height a release name states (`WEBDL-1080p`, `[2160p]`, `4K`).
fn name_height(stem: &str) -> Option<u32> {
    let caps = HEIGHT_RE.as_ref()?.captures(stem)?;
    if let Some(lines) = caps.get(1) {
        return lines.as_str().parse().ok();
    }
    match caps.get(2)?.as_str().to_ascii_lowercase().as_str() {
        "8k" => Some(4320),
        _ => Some(2160),
    }
}

/// What sets `copy` apart from `primary` by name: the words its stem adds
/// (`WEBDL-1080p`), else its container (`AVI`), else the nearest folder whose
/// name differs (`Season 3`), else its own resolution tag, else `Copy`.
fn copy_label(primary: &Path, copy: &Path) -> String {
    let stem = |p: &Path| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };
    let (a, b) = (stem(primary), stem(copy));
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| c.is_whitespace() || "._[](){},".contains(c))
            .filter(|w| w.chars().any(char::is_alphanumeric))
            .map(str::to_string)
            .collect()
    };
    let have: std::collections::HashSet<String> = words(&a).iter().map(|w| fold_name(w)).collect();
    let added: Vec<String> = words(&b)
        .into_iter()
        .filter(|w| !have.contains(&fold_name(w)))
        .collect();
    if !added.is_empty() {
        return added.join(" ");
    }
    let ext = |p: &Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_uppercase)
    };
    if ext(primary) != ext(copy) {
        if let Some(e) = ext(copy) {
            return e;
        }
    }
    let (mut pa, mut pb) = (primary.parent(), copy.parent());
    while let (Some(x), Some(y)) = (pa, pb) {
        match (
            x.file_name().and_then(|s| s.to_str()),
            y.file_name().and_then(|s| s.to_str()),
        ) {
            (Some(nx), Some(ny)) if nx != ny => return ny.to_string(),
            (Some(_), Some(_)) => (pa, pb) = (x.parent(), y.parent()),
            _ => break,
        }
    }
    words(&b)
        .into_iter()
        .rev()
        .find(|w| name_height(w).is_some())
        .unwrap_or_else(|| "Copy".to_string())
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
pub(crate) fn resolve_item_3d_format(
    path: &Path,
    kind: MediaKind,
    probe: &mut pharos_core::MediaProbe,
) {
    if !matches!(kind, MediaKind::Movie | MediaKind::Episode) {
        return;
    }

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let mut explicit_fmt = crate::metadata::filename::has_explicit_full_or_half_3d_token(stem);
    let (mut fn_fmt, mut fn_label) = crate::metadata::filename::parse_video_3d_format(stem);

    if fn_fmt.is_none() && explicit_fmt.is_none() {
        if let Some(parent_name) = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
        {
            if explicit_fmt.is_none() {
                explicit_fmt =
                    crate::metadata::filename::has_explicit_full_or_half_3d_token(parent_name);
            }
            if fn_fmt.is_none() {
                let (p_fmt, p_label) =
                    crate::metadata::filename::parse_video_3d_format(parent_name);
                fn_fmt = p_fmt;
                if fn_label.is_none() {
                    fn_label = p_label;
                }
            }
        }
    }

    let (final_fmt, source) = if let Some(probe_fmt) = probe.video_3d_format {
        let probe_source = if probe_fmt == Video3DFormat::Mvc {
            Detect3DSource::ProbeMvc
        } else if probe
            .container
            .as_deref()
            .is_some_and(|c| c.contains("mp4") || c.contains("mov") || c.contains("m4v"))
            || path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                e.eq_ignore_ascii_case("mp4")
                    || e.eq_ignore_ascii_case("mov")
                    || e.eq_ignore_ascii_case("m4v")
            })
        {
            Detect3DSource::ProbeSideData
        } else {
            Detect3DSource::ProbeTag
        };

        if probe_fmt.is_packed_sbs_or_tab() {
            if let Some(explicit) = explicit_fmt {
                let is_same_axis = {
                    let is_sbs = |f: Video3DFormat| {
                        matches!(
                            f,
                            Video3DFormat::HalfSideBySide | Video3DFormat::FullSideBySide
                        )
                    };
                    let is_tab = |f: Video3DFormat| {
                        matches!(
                            f,
                            Video3DFormat::HalfTopAndBottom | Video3DFormat::FullTopAndBottom
                        )
                    };
                    (is_sbs(probe_fmt) && is_sbs(explicit))
                        || (is_tab(probe_fmt) && is_tab(explicit))
                };
                if is_same_axis && explicit != probe_fmt {
                    (explicit, probe_source)
                } else {
                    (probe_fmt, probe_source)
                }
            } else {
                (probe_fmt, probe_source)
            }
        } else {
            (probe_fmt, probe_source)
        }
    } else if let Some(fn_fmt) = fn_fmt {
        (fn_fmt, Detect3DSource::Filename)
    } else {
        if let Some(label) = fn_label {
            if label == "anaglyph" || label == "unspecified_3d" {
                metrics::counter!(
                    "pharos_3d_detect_total",
                    "format" => label,
                    "source" => Detect3DSource::Filename.label(),
                    "media_kind" => kind.as_str(),
                )
                .increment(1);
            }
        }
        return;
    };

    probe.video_3d_format = Some(final_fmt);

    metrics::counter!(
        "pharos_3d_detect_total",
        "format" => final_fmt.wire_name(),
        "source" => source.label(),
        "media_kind" => kind.as_str(),
    )
    .increment(1);

    tracing::info!(
        target: "pharos::scanner::3d",
        path = %path.display(),
        video_3d_format = final_fmt.wire_name(),
        source = source.label(),
        width = probe.width,
        height = probe.height,
        "detected 3D video format"
    );
}

pub fn is_episode_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if has_sxxeyy_token(name) || has_nxnn_token(name) {
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

/// How far above a video the scanner looks for a show's `tvshow.nfo`: the
/// file's own folder, a season or extras folder, and a disc folder in that.
const SHOW_NFO_DEPTH: usize = 3;

/// The show folder a video sits in, marked by the `tvshow.nfo` that Kodi,
/// Jellyfin and Sonarr write at a show's root.
pub fn show_folder_of(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .take(SHOW_NFO_DEPTH)
        .find(|p| holds_show_nfo(p))
        .map(Path::to_path_buf)
}

/// Folders already asked whether they hold `tvshow.nfo`, so a scan stats each
/// folder once rather than three times for every file in it. Cleared when a
/// scan starts ([`forget_show_nfos`]), so a `tvshow.nfo` added since is seen.
static SHOW_NFO_FOLDERS: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, bool>>> =
    std::sync::LazyLock::new(Default::default);

fn holds_show_nfo(dir: &Path) -> bool {
    let cached = SHOW_NFO_FOLDERS
        .lock()
        .ok()
        .and_then(|m| m.get(dir).copied());
    if let Some(held) = cached {
        return held;
    }
    let held = dir.join("tvshow.nfo").is_file();
    if let Ok(mut m) = SHOW_NFO_FOLDERS.lock() {
        m.insert(dir.to_path_buf(), held);
    }
    held
}

/// Drop what [`holds_show_nfo`] remembered.
pub(crate) fn forget_show_nfos() {
    if let Ok(mut m) = SHOW_NFO_FOLDERS.lock() {
        m.clear();
    }
}

/// Whether a video is an episode: by its name or season folder
/// ([`is_episode_path`]), or by the show folder it sits in.
pub fn is_episode_file(path: &Path) -> bool {
    is_episode_path(path) || show_folder_of(path).is_some()
}

/// A video that is an episode only by the show folder above it.
pub(crate) fn is_show_folder_video(path: &Path) -> bool {
    let video = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| FILM_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    video && show_folder_of(path).is_some()
}

/// A folder of a show's bonus material (`Extras`, `Chuck Extras`,
/// `Featurettes`, `Special Features`, `Bonus`).
fn is_extras_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words
        .iter()
        .any(|w| matches!(*w, "extras" | "featurettes" | "bonus" | "bonuses"))
        || words.windows(2).any(|w| w == ["special", "features"])
}

/// [`parse_series_info`] for a file under a show folder: the show is the
/// folder holding `tvshow.nfo`, not a sub-folder between it and the file
/// (`SAO II/`, `Chuck Extras/`), and a file in an extras folder is a special
/// with no episode number, since its name carries none of the show's.
pub fn episode_series_info(path: &Path) -> Option<SeriesInfo> {
    let mut info = parse_series_info(path)?;
    let Some(show) = show_folder_of(path) else {
        return Some(info);
    };
    let extras = path
        .ancestors()
        .skip(1)
        .take_while(|p| *p != show)
        .filter_map(|p| p.file_name()?.to_str())
        .any(is_extras_dir);
    if extras {
        info.season_number = Some(0);
        info.episode_number = None;
    }
    let folder = show.to_string_lossy().into_owned();
    if info.series_folder.as_deref() != Some(folder.as_str()) {
        let name = show.file_name().and_then(|s| s.to_str()).unwrap_or(&folder);
        info.series_year = parse_folder_year(name);
        info.series_name = match info.series_year {
            Some(_) => strip_trailing_year(name).to_string(),
            None => name.to_string(),
        };
        tracing::debug!(
            path = %path.display(),
            show = %folder,
            parsed = ?info.series_folder,
            "scanner: episode keyed to the show folder holding tvshow.nfo"
        );
        info.series_folder = Some(folder);
    }
    Some(info)
}

/// B587 — whether `name` holds a Kodi `1x16` episode token: one or two
/// season digits, `x`, then two or three episode digits, standing alone
/// (`- 1x16 -`, `.1x01x02.`). Fewer season digits than a frame size keeps
/// `1920x1080` out, and two episode digits keep a title's `2x4` out. A
/// two-digit season must be followed by ` - ` as in `Show - 10x05 - Title`,
/// so a film named `Ocean 10x10` stays a film. `parse_series_info` already
/// reads the numbers from this form; without this, a file holding it
/// outside a season folder was stored as a movie.
pub fn has_nxnn_token(name: &str) -> bool {
    let b = name.as_bytes();
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    // A non-ASCII byte is part of a letter, so it is no word boundary.
    let separator = |c: &u8| c.is_ascii() && !c.is_ascii_alphanumeric();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || separator(&b[i - 1])) {
            let season = digits(i);
            let x = i + season;
            if (1..=2).contains(&season) && b.get(x).is_some_and(|c| c.eq_ignore_ascii_case(&b'x'))
            {
                let episode = digits(x + 1);
                let end = x + 1 + episode;
                let stands_alone = b
                    .get(end)
                    .map_or(true, |c| separator(c) || c.eq_ignore_ascii_case(&b'x'));
                let titled = season == 1 || b[end..].starts_with(b" - ");
                if (2..=3).contains(&episode) && stands_alone && titled {
                    return true;
                }
            }
            i += season;
            continue;
        }
        i += 1;
    }
    false
}

/// Whether `name` holds an `S01E02` token at a word start.
pub fn has_sxxeyy_token(name: &str) -> bool {
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
///   when nothing else fits. A scene release folder
///   (`Show.Name.S01E03.720p-GRP`) is skipped like a season folder and
///   names the show instead; the folder above it is the show's only when
///   its name matches, else the show is keyed beside the release.
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
    // first non-season, non-release ancestor's FULL path becomes the C11
    // folder key.
    let parents: Vec<&Path> = path.ancestors().skip(1).collect();

    let mut season_from_dir: Option<u32> = None;
    let mut series_name: Option<String> = None;
    let mut series_folder: Option<PathBuf> = None;
    // The innermost release folder names the show; the outermost one is
    // where a show without its own folder is keyed (beside it).
    let mut release: Option<ReleaseFolder> = None;
    let mut outermost_release: Option<&Path> = None;

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
        if let Some(found) = parse_release_folder(component) {
            season_from_dir = season_from_dir.or(found.season);
            release.get_or_insert(found);
            outermost_release = Some(parent);
            continue;
        }
        // Above a release, a folder is the show's own only when it carries
        // the show's name; otherwise it is the library root.
        if let Some(found) = &release {
            if fold_name(strip_trailing_year(component)) != fold_name(&found.show) {
                break;
            }
        }
        // First non-season ancestor wins as the series. LIB-C11: capture
        // both the display name AND the canonical folder path (the show
        // root that holds the season dirs / episodes) for identity.
        series_name = Some(component.to_string());
        series_folder = Some(parent.to_path_buf());
        break;
    }

    let release_year = release.as_ref().and_then(|r| r.year);
    if series_name.is_none() {
        if let Some(found) = release {
            series_folder = outermost_release
                .and_then(Path::parent)
                .map(|p| p.join(&found.show));
            series_name = Some(found.show);
        }
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
        .and_then(parse_folder_year)
        .or(release_year);
    // When a year was parsed, strip the trailing `(YYYY)` from the display
    // name so the title stays clean ("Show A", not "Show A (1961)") while
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
        series_key: None,
    })
}

/// The show a scene release folder (`Show.Name.S01E03.720p.HDTV-GRP`,
/// `Show.Name.2005.S02.1080p.BluRay-GRP`) is for: the cleaned name before
/// its season/episode token, the token's season, and any year in the name.
struct ReleaseFolder {
    show: String,
    season: Option<u32>,
    year: Option<u32>,
}

/// A season-pack token (`.S02.`) in a release folder name: a bare season
/// with no episode, between separators.
static SEASON_PACK_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)[ ._\[(-]s(?<s>\d{1,2})(?:[ ._\])-]|$)")
        .map_err(|e| tracing::error!(error = %e, "invalid season-pack regex"))
        .ok()
});

/// `Some` when `name` is a release folder rather than a show folder: it
/// holds a season+episode or season-pack token with a show name before it.
/// Episode-only and absolute-number forms are not enough, since a show
/// folder may itself end in a number ("Show 99").
fn parse_release_folder(name: &str) -> Option<ReleaseFolder> {
    let token = EP_EXPRS
        .iter()
        .filter(|x| !x.optimistic)
        .filter_map(|x| x.re.captures(name))
        .find(|c| {
            c.name("s")
                .and_then(|m| plausible_season(m.as_str()))
                .is_some()
        })
        .or_else(|| SEASON_PACK_RE.as_ref()?.captures(name))?;
    let start = token.get(0)?.start();
    let season = token.name("s").and_then(|m| plausible_season(m.as_str()));
    let parsed = crate::metadata::filename::parse_filename(&name[..start], true);
    Some(ReleaseFolder {
        show: parsed.title?,
        season,
        year: parsed.year,
    })
}

/// Lower-cased letters and digits only, so `Person of Interest` and
/// `Person.of.Interest` compare equal.
fn fold_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
/// (`Show A (1961)` → `1961`). Returns `None` when the folder carries no
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
        compile(r"(?i)(?:^|[\\/._ \[(-])ep_?(?<e>\d+)", false),
        // Episode 12 (word, no season).
        compile(r"(?i)episode[ ._-]*(?<e>\d+)", false),
        // E12 at a separator boundary (Show E01, name.E05.).
        compile(r"(?i)(?:^|[\\/._ \[(-])e(?<e>\d+)", false),
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
                // A half-numbered recap (`SAO II - 14.5`) is not episode 14.
                let half = name.as_bytes().get(e_match.end() + 1..e_match.end() + 3);
                if next == b'.'
                    && half.is_some_and(|h| h[0].is_ascii_digit() && !h[1].is_ascii_alphanumeric())
                {
                    continue;
                }
            }
            let Ok(episode) = e_match.as_str().parse::<u32>() else {
                continue;
            };
            let season = caps.name("s").and_then(|m| plausible_season(m.as_str()));
            return Some((season, episode));
        }
    }
    None
}

/// A season number outside Jellyfin's junk band (200–1927 or above 2500,
/// where resolutions and years land: `1920x1080` is not season 1920).
fn plausible_season(digits: &str) -> Option<u32> {
    digits
        .parse::<u32>()
        .ok()
        .filter(|s| !(200..=1927).contains(s) && *s <= 2500)
}

/// Where the episode number ends in a filename, for the strict (non-
/// optimistic) episode forms: `S03E07`, `1x07`, `E07`, `Episode 7`. The
/// episode's own title, when the name carries one, follows it.
pub(crate) fn episode_token_end(name: &str) -> Option<usize> {
    EP_EXPRS
        .iter()
        .filter(|x| !x.optimistic)
        .filter_map(|x| x.re.captures(name)?.name("e"))
        .find(|e| {
            !name
                .as_bytes()
                .get(e.end())
                .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'i' | b'I' | b'p' | b'P'))
        })
        .map(|e| e.end())
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

/// Whether this entry is a filesystem artefact wearing a media extension.
///
/// The walk recognises files by extension alone, which is right for real media
/// and wrong for two things that carry one anyway:
///
/// - **AppleDouble sidecars** (`._Foo.mp4`). macOS writes one beside every file
///   it copies to a filesystem without native extended-attribute support, and it
///   inherits the original's name — extension included. They are a few KB of
///   attribute data, never media, and ffmpeg rejects them with the same
///   `Invalid data found` an actually-corrupt file gets, so they are
///   indistinguishable from real damage in the log. One is in the library today.
/// - **Hidden files** generally. A leading dot means "not for browsing", and
///   cataloguing one surfaces it in the UI as an ordinary title. Jellyfin skips
///   these too, so this also removes a parity difference.
///
/// Filtering at the WALK rather than at the probe matters: a skipped entry costs
/// nothing, whereas reaching the prober costs a whole-file open over NFS and a
/// background-I/O permit, on something that was never going to parse.
fn is_sidecar_noise(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// A release's sample clip: a video under a `Sample` folder, or one whose
/// name ends in `sample` (`Heat.1995.sample.mkv`, `Heat (Sample).mkv`).
/// Indexed, each showed up as another copy of its film.
pub(crate) fn is_release_sample(relative: &Path) -> bool {
    let video = relative
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| FILM_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if !video {
        return false;
    }
    let in_sample_dir = relative.parent().is_some_and(|dir| {
        dir.components().any(|c| {
            c.as_os_str().to_str().is_some_and(|n| {
                n.eq_ignore_ascii_case("sample") || n.eq_ignore_ascii_case("samples")
            })
        })
    });
    in_sample_dir
        || relative
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| SAMPLE_STEM_RE.as_ref().is_some_and(|re| re.is_match(stem)))
}

/// A release group's advert clip packed beside its films (`ETRG.mp4`):
/// a video named only for the group.
pub(crate) fn is_release_promo(path: &Path) -> bool {
    let video = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| FILM_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    video
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| {
                RELEASE_PROMO_STEMS
                    .iter()
                    .any(|g| stem.eq_ignore_ascii_case(g))
            })
}

/// Release groups known to pack an advert clip named after themselves.
const RELEASE_PROMO_STEMS: &[&str] = &["ETRG", "RARBG", "YIFY", "YTS", "YTS.AG", "YTS.MX"];

/// A file stem that starts with `sample-`, or ends in the word `sample`, bare
/// or bracketed.
static SAMPLE_STEM_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)^sample[._\-]|(?:^|[ ._\-(\[])sample[)\]]?$").ok()
});

/// Jellyfin's marker file: a folder holding `.ignore` is left out of the
/// library with everything beneath it, so a `To Sort/` of unsorted rips
/// stays off the shelves until it is sorted.
pub const IGNORE_MARKER: &str = ".ignore";

fn is_ignored_dir(dir: &Path) -> bool {
    dir.join(IGNORE_MARKER).is_file()
}

/// Whether `path` sits in a folder under `root` that holds [`IGNORE_MARKER`].
pub fn in_ignored_folder(path: &Path, root: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .take_while(|p| p.starts_with(root))
        .any(is_ignored_dir)
}

/// Whether writing `path` changes rows already indexed beside it: an
/// [`IGNORE_MARKER`] hides them, a `tvshow.nfo` turns films into episodes.
/// The watcher must reconcile for these rather than add.
#[cfg_attr(not(feature = "watch"), allow(dead_code))]
pub(crate) fn reshapes_indexed_rows(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == IGNORE_MARKER || n.eq_ignore_ascii_case("tvshow.nfo"))
}

/// Whether the full walk leaves `path` out: under a `.ignore` folder, a
/// release sample or a release group's promo clip (V961, V969, V972).
#[cfg_attr(not(feature = "watch"), allow(dead_code))]
pub(crate) fn walk_skips(path: &Path, root: &Path) -> bool {
    in_ignored_folder(path, root)
        || is_release_promo(path)
        || is_release_sample(path.strip_prefix(root).unwrap_or(path))
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
    let (outcome, ignored_folders) =
        tokio::task::spawn_blocking(move || -> DomainResult<(WalkOutcome, u64)> {
            let mut out = Vec::new();
            let mut errors = 0usize;
            let mut ignored_folders = 0u64;
            let entries = walkdir::WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    let ignored = e.file_type().is_dir() && is_ignored_dir(e.path());
                    if ignored {
                        tracing::info!(
                            path = %e.path().display(),
                            "walk: folder holds .ignore; leaving it out of the library"
                        );
                        ignored_folders += 1;
                    }
                    !ignored
                });
            for entry in entries {
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
                if is_sidecar_noise(e.path()) {
                    continue;
                }
                if is_release_promo(e.path()) {
                    tracing::debug!(path = %e.path().display(), "walk: skipping a release group's promo clip");
                    metrics::counter!("pharos_scan_skipped_files_total", "reason" => "promo")
                        .increment(1);
                    continue;
                }
                if is_release_sample(e.path().strip_prefix(&root).unwrap_or(e.path())) {
                    tracing::debug!(path = %e.path().display(), "walk: skipping a release sample");
                    metrics::counter!("pharos_scan_skipped_files_total", "reason" => "sample")
                        .increment(1);
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
            Ok((WalkOutcome { files: out, errors }, ignored_folders))
        })
        .await
        .map_err(|e| DomainError::Backend(format!("walk join: {e}")))??;
    // Counted out here: the walk's blocking thread has no task-local recorder.
    metrics::counter!("pharos_scan_ignored_folders_total").increment(ignored_folders);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    /// Spec 033 (V940) — a reset drops what an enrichment wrote and keeps
    /// what the files say.
    #[tokio::test]
    async fn reset_to_local_metadata_keeps_the_files_and_drops_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let show = dir.path().join("Show A");
        std::fs::create_dir_all(&show).unwrap();
        let with_nfo = show.join("Show A - S01E01.mkv");
        std::fs::write(&with_nfo, b"").unwrap();
        std::fs::write(
            show.join("Show A - S01E01.nfo"),
            "<episodedetails><title>The Old Second</title>\
             <showtitle>Show A</showtitle><plot>Overview.</plot></episodedetails>",
        )
        .unwrap();
        let stale = |path: &std::path::Path| {
            let mut item = MediaItem {
                path: path.to_path_buf(),
                title: "The New Start in the Milky Way".into(),
                kind: MediaKind::Episode,
                series: Some(SeriesInfo {
                    series_name: "Show A".into(),
                    season_number: Some(1),
                    episode_number: Some(1),
                    series_folder: Some(show.to_string_lossy().into_owned()),
                    series_year: None,
                    series_key: None,
                }),
                ..MediaItem::default()
            };
            item.metadata.overview = Some("Tyson.".into());
            item.metadata.premiere_date = Some(1_394_323_200);
            item.metadata.community_rating = Some(9.1);
            item.metadata.provider_ids.tvdb = Some("4847410".into());
            item
        };
        let mut item = stale(&with_nfo);
        reset_to_local_metadata(&mut item).await;
        assert_eq!(item.title, "The Old Second");
        assert_eq!(item.metadata.overview.as_deref(), Some("Overview."));
        assert_eq!(item.metadata.premiere_date, None);
        assert_eq!(item.metadata.community_rating, None);
        assert_eq!(
            item.metadata.provider_ids,
            pharos_core::ProviderIds::default()
        );

        let bare = show.join("Show A - S01E02 - The Old Second.mkv");
        std::fs::write(&bare, b"").unwrap();
        let mut item = stale(&bare);
        reset_to_local_metadata(&mut item).await;
        assert_ne!(item.title, "The New Start in the Milky Way");
        assert_eq!(item.metadata.overview, None);
        assert_eq!(item.metadata.provider_ids.tvdb, None);
    }

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
        // Size of every `put_batch` call, in order; single `put`s are not here.
        put_batches: Mutex<Vec<usize>>,
        // Ids whose writes are refused, like a row the database rejects.
        reject_ids: Mutex<HashSet<MediaId>>,
        // Number of `alternate_sources` reads, to prove the skip path avoids them.
        alternate_sources_calls: AtomicUsize,
        // Every `merge_user_data(from, into, resume)` call, in order.
        merged: Mutex<Vec<(MediaId, MediaId, pharos_core::FoldResume)>>,
        // Spec 033 — series_overrides mirror.
        overrides: Mutex<Vec<pharos_core::SeriesOverride>>,
        /// Paths `mark_reconcile_paths` marked, in order.
        reconcile_marked: Mutex<Vec<String>>,
    }

    impl MediaStore for MemStore {
        async fn put_batch(&self, items: &[MediaItem]) -> DomainResult<()> {
            self.put_batches.lock().unwrap().push(items.len());
            let reject = self.reject_ids.lock().unwrap().clone();
            let mut inner = self.inner.lock().unwrap();
            let mut first_err = None;
            for item in items {
                if reject.contains(&item.id) {
                    first_err.get_or_insert(DomainError::Backend(format!("rejected {}", item.id)));
                } else {
                    inner.insert(item.id, item.clone());
                }
            }
            first_err.map_or(Ok(()), Err)
        }
        async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
            self.inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .cloned()
                .ok_or(DomainError::NotFound(id))
        }
        async fn put(&self, item: MediaItem) -> DomainResult<()> {
            if self.reject_ids.lock().unwrap().contains(&item.id) {
                return Err(DomainError::Backend(format!("rejected {}", item.id)));
            }
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
        async fn id_at_path(&self, path: &Path) -> DomainResult<Option<MediaId>> {
            Ok(self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .values()
                .find(|item| item.path == path)
                .map(|item| item.id))
        }
        async fn scan_state(&self, id: MediaId) -> DomainResult<Option<ScanState>> {
            let has_editions = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .is_some_and(|item| !item.probe.alternate_sources.is_empty());
            Ok(self
                .states
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .map(|state| ScanState {
                    has_editions,
                    ..*state
                }))
        }
        async fn alternate_sources(
            &self,
            id: MediaId,
        ) -> DomainResult<Vec<pharos_core::AlternateMediaSource>> {
            self.alternate_sources_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .map(|item| item.probe.alternate_sources.clone())
                .unwrap_or_default())
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
            //
            // The row's own kind picks the schema version, exactly as the
            // real store's `CASE WHEN kind = 'book'` does — a fake that
            // stamped the probe version onto a book would hide the
            // re-read-forever bug the real one is written to avoid.
            let kind = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&id)
                .map(|it| it.kind);
            let Some(kind) = kind else {
                return Ok(());
            };
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
                        // Derived from the row at read time in `scan_state`.
                        has_editions: false,
                        // Mirror the store: mark_seen stamps the version of
                        // the pipeline that produced the row, which for a book
                        // is the book parser (005-kindle-conversion).
                        probe_schema_version: if kind == pharos_core::MediaKind::Book {
                            pharos_core::BOOK_SCHEMA_VERSION
                        } else {
                            pharos_core::PROBE_SCHEMA_VERSION
                        },
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
        async fn merge_user_data(
            &self,
            from: MediaId,
            into: MediaId,
            resume: pharos_core::FoldResume,
        ) -> DomainResult<()> {
            self.merged
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .push((from, into, resume));
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

        async fn bump_art_version(&self, item_id: MediaId) -> DomainResult<()> {
            if let Some(it) = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get_mut(&item_id)
            {
                it.art_version += 1;
            }
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

        async fn set_item_match(
            &self,
            item_id: MediaId,
            provider: &str,
            external_id: &str,
            source: &str,
            confidence: Option<f32>,
            refreshed_at: i64,
        ) -> DomainResult<()> {
            // No-op when the row is absent, mirroring rebind_path/set_artwork.
            if let Some(item) = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get_mut(&item_id)
            {
                item.match_provider = Some(provider.to_string());
                item.match_external_id = Some(external_id.to_string());
                item.match_source = Some(source.to_string());
                item.match_confidence = confidence;
                item.metadata_refreshed_at = Some(refreshed_at);
            }
            Ok(())
        }

        async fn items_needing_match(
            &self,
            limit: i64,
            ttl_cutoff: i64,
        ) -> DomainResult<Vec<MediaItem>> {
            let inner = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut matches: Vec<MediaItem> = inner
                .values()
                .filter(|item| {
                    let source_eligible = matches!(
                        item.match_source.as_deref(),
                        None | Some("search") | Some("none")
                    );
                    let ttl_eligible = item
                        .metadata_refreshed_at
                        .is_none_or(|refreshed| refreshed < ttl_cutoff);
                    let kind_eligible = matches!(item.kind, MediaKind::Movie | MediaKind::Episode);
                    source_eligible && ttl_eligible && kind_eligible
                })
                .cloned()
                .collect();
            matches.sort_by_key(|item| item.id);
            matches.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            Ok(matches)
        }

        async fn audio_items_needing_art(
            &self,
            limit: i64,
            ttl_cutoff: i64,
            current_miss_marker: &str,
        ) -> DomainResult<Vec<MediaItem>> {
            let inner = self
                .inner
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut matches: Vec<MediaItem> = inner
                .values()
                .filter(|item| {
                    let source_eligible = matches!(
                        item.match_source.as_deref(),
                        None | Some("search") | Some("none")
                    );
                    let ttl_eligible = item
                        .metadata_refreshed_at
                        .is_none_or(|refreshed| refreshed < ttl_cutoff);
                    // A miss stamped by an older query version is re-admitted
                    // regardless of the TTL — see `audio_items_needing_art`.
                    let stale_verdict = item.match_source.as_deref() == Some("none")
                        && item.match_external_id.as_deref() != Some(current_miss_marker);
                    matches!(item.kind, MediaKind::Audio)
                        && !item.has_primary_art
                        && source_eligible
                        && (ttl_eligible || stale_verdict)
                })
                .cloned()
                .collect();
            matches.sort_by_key(|item| item.id);
            matches.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            Ok(matches)
        }

        async fn clear_provider_artwork(&self, _provider: &str) -> DomainResult<u64> {
            // MemStore is a lightweight test-double with no artwork table; the
            // invalidation path is exercised against the real sqlx stores.
            Ok(0)
        }

        async fn servable_artwork_page(
            &self,
            _after: Option<(MediaId, String)>,
            _limit: i64,
        ) -> DomainResult<Vec<pharos_core::ArtworkRow>> {
            Ok(Vec::new())
        }

        async fn remove_dead_artwork(
            &self,
            _item_id: MediaId,
            _role: &str,
            _readmit: bool,
        ) -> DomainResult<()> {
            Ok(())
        }

        async fn item_entity_counts(
            &self,
            item_id: MediaId,
        ) -> DomainResult<pharos_core::EntityCounts> {
            let genres = self
                .item_genres
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&item_id)
                .map_or(0, |v| v.len());
            let people = self
                .item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&item_id)
                .map_or(0, |v| v.len());
            let studios = self
                .item_studios
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?
                .get(&item_id)
                .map_or(0, |v| v.len());
            Ok(pharos_core::EntityCounts {
                genres: genres as u32,
                people: people as u32,
                studios: studios as u32,
            })
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
        async fn credits_for_person(
            &self,
            wire_id: &str,
        ) -> DomainResult<Vec<(MediaId, pharos_core::ItemPerson)>> {
            let links = self
                .item_people
                .lock()
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let mut credits: Vec<(MediaId, pharos_core::ItemPerson)> = links
                .iter()
                .flat_map(|(id, people)| people.iter().map(move |p| (*id, p)))
                .filter(|(_, p)| pharos_core::person_wire_id(&p.name) == wire_id)
                .map(|(id, p)| {
                    (
                        id,
                        pharos_core::ItemPerson {
                            name: p.name.clone(),
                            wire_id: wire_id.to_string(),
                            role: p.role.clone(),
                            character: p.character.clone(),
                            kind: p.kind,
                            sort_order: p.sort_order,
                            thumb_url: p.thumb.clone(),
                        },
                    )
                })
                .collect();
            credits.sort_by_key(|(id, _)| *id);
            Ok(credits)
        }
        async fn actors_named(
            &self,
            _words: &[String],
            _limit: u32,
        ) -> DomainResult<Vec<pharos_core::Person>> {
            Ok(Vec::new())
        }
        async fn person_details(
            &self,
            _wire_id: &str,
        ) -> DomainResult<Option<pharos_core::PersonDetails>> {
            Ok(None)
        }
        async fn put_person_details(
            &self,
            _wire_id: &str,
            _details: &pharos_core::PersonDetails,
        ) -> DomainResult<()> {
            Ok(())
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
        async fn actor_credits_for_items(
            &self,
            _items: &[MediaId],
        ) -> DomainResult<Vec<(MediaId, pharos_core::ItemPerson)>> {
            Ok(vec![])
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

    impl pharos_core::SeriesIdentityStore for MemStore {
        async fn series_overrides(
            &self,
            folders: &[String],
        ) -> DomainResult<Vec<pharos_core::SeriesOverride>> {
            Ok(self
                .overrides
                .lock()
                .unwrap()
                .iter()
                .filter(|o| folders.contains(&o.series_folder))
                .cloned()
                .collect())
        }

        async fn insert_series_override(
            &self,
            o: pharos_core::SeriesOverride,
        ) -> DomainResult<bool> {
            let mut all = self.overrides.lock().unwrap();
            if all.iter().any(|x| {
                x.source == o.source
                    && x.path == o.path
                    && (o.path.is_some() || x.series_folder == o.series_folder)
            }) {
                return Ok(false);
            }
            all.push(o);
            Ok(true)
        }

        async fn series_identity_rows(
            &self,
            root_prefix: &str,
        ) -> DomainResult<Vec<pharos_core::SeriesIdentityRow>> {
            Ok(self
                .inner
                .lock()
                .unwrap()
                .values()
                .filter(|i| i.path.starts_with(root_prefix))
                .filter_map(|i| {
                    let s = i.series.as_ref()?;
                    Some(pharos_core::SeriesIdentityRow {
                        path: i.path.to_string_lossy().into_owned(),
                        series_folder: s.series_folder.clone(),
                        series_key: s.series_key.clone(),
                        series_year: s.series_year,
                    })
                })
                .collect())
        }

        async fn set_series_keys(
            &self,
            updates: &[pharos_core::SeriesKeyUpdate],
        ) -> DomainResult<u64> {
            let mut n = 0;
            for item in self.inner.lock().unwrap().values_mut() {
                let Some(u) = updates.iter().find(|u| Path::new(&u.path) == item.path) else {
                    continue;
                };
                if let Some(s) = item.series.as_mut() {
                    s.series_key.clone_from(&u.series_key);
                    s.series_year = u.series_year;
                    n += 1;
                }
            }
            Ok(n)
        }

        async fn mark_reconcile_paths(&self, paths: &[String], _at: i64) -> DomainResult<u64> {
            self.reconcile_marked
                .lock()
                .unwrap()
                .extend(paths.iter().cloned());
            Ok(paths.len() as u64)
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
        // Every probe takes this long, to model a slow disk.
        delay: Option<std::time::Duration>,
        // Raise the flag during the Nth probe call (1-based).
        cancel_during: Option<(usize, Arc<std::sync::atomic::AtomicBool>)>,
        // Overwrite the file with these bytes while it is being probed, as a
        // concurrent writer would.
        rewrite_during: Option<(String, Vec<u8>)>,
    }

    impl Prober for FakeProber {
        async fn probe(&self, path: &Path) -> DomainResult<ProbeInfo> {
            let nth = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            if let Some((n, flag)) = &self.cancel_during {
                if nth == *n {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            if let Some((needle, bytes)) = &self.rewrite_during {
                if path.to_string_lossy().contains(needle.as_str()) {
                    std::fs::write(path, bytes).unwrap();
                }
            }
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

    /// SC-002 — a book path must never reach the prober. This is the assertion
    /// the whole scan design turns on: `probe_one` returns `None` when the
    /// prober fails (V6), so an epub handed to ffmpeg is an item that never
    /// exists. The count, not the import, is what proves the ordering.
    ///
    /// Verified red by moving the `book_item` branch below the
    /// `self.prober.probe(&path)` call: the count goes to 5 — every file
    /// probed, books included — against the 1 asserted here. In production
    /// that probe does not merely waste a fork: it FAILS on an epub, and the
    /// `Err` arm writes nothing, so the book silently never appears.
    #[tokio::test]
    async fn a_book_path_never_reaches_the_prober() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Dune.epub").await;
        touch(td.path(), "Watchmen.cbz").await;
        touch(td.path(), "Manual.pdf").await;
        touch(td.path(), "Sideloaded.azw3").await;
        // One real media file, so the test also proves the branch did not
        // swallow the probe path wholesale.
        touch(td.path(), "movie.mkv").await;

        let calls = Arc::new(AtomicUsize::new(0));
        let prober = FakeProber {
            calls: calls.clone(),
            ..Default::default()
        };
        let items = FsScanner::new(prober).scan(td.path()).await.unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly the ONE media file may be probed; every book must bypass ffmpeg entirely"
        );
        assert_eq!(items.len(), 5, "all four books plus the movie must import");

        let books: Vec<_> = items.iter().filter(|i| i.kind == MediaKind::Book).collect();
        assert_eq!(books.len(), 4, "four book files, four Book items");
        for b in &books {
            assert!(
                b.book.is_some(),
                "{} imported as Book but carries no BookMeta",
                b.title
            );
            assert_eq!(
                b.probe.duration_ms, None,
                "a book has no duration; MediaProbe must stay default apart from size"
            );
        }

        // Formats are classified, not guessed uniformly.
        let by_title = |t: &str| {
            items
                .iter()
                .find(|i| i.title == t)
                .and_then(|i| i.book.as_ref())
                .map(|b| b.format)
        };
        assert_eq!(by_title("Dune"), Some(pharos_core::BookFormat::Epub));
        assert_eq!(by_title("Watchmen"), Some(pharos_core::BookFormat::Comic));
        assert_eq!(by_title("Manual"), Some(pharos_core::BookFormat::Pdf));
        assert_eq!(
            by_title("Sideloaded"),
            Some(pharos_core::BookFormat::Unreadable),
            ".azw3 is indexed but not readable — recorded as such, not as an epub"
        );

        // The movie still went through the prober unchanged.
        assert_eq!(
            items.iter().filter(|i| i.kind == MediaKind::Movie).count(),
            1
        );
    }

    /// FR-007 — a book with no metadata inside it still gets a title, from the
    /// filename stem. Nothing may list untitled.
    #[tokio::test]
    async fn a_book_with_no_internal_metadata_is_titled_from_its_filename() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "The Left Hand of Darkness.epub").await;
        let items = FsScanner::new(FakeProber::default())
            .scan(td.path())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "The Left Hand of Darkness");
        assert_eq!(items[0].kind, MediaKind::Book);
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

    /// A file that cannot be probed is re-read on EVERY scan today, because a
    /// failed probe writes no row and the incremental skip has nothing to
    /// compare against. With a memo the second pass does not touch it.
    ///
    /// The prober's CALL COUNT is the assertion, not the item count: the item
    /// count is 1 either way, which is exactly why this went unnoticed.
    #[tokio::test]
    async fn a_file_that_cannot_be_probed_is_not_re_probed_next_scan() {
        let td = TempDir::new().unwrap();
        let memo_dir = TempDir::new().unwrap();
        touch(td.path(), "good.mkv").await;
        touch(td.path(), "bad.mkv").await;
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            force_fail_for: Some("bad".into()),
            genre: None,
            ..Default::default()
        };
        let memo = Arc::new(crate::probe_memo::ProbeMemo::load(
            memo_dir.path().join("probe_failures.json"),
            7,
        ));
        let store = MemStore::default();

        FsScanner::new(prober.clone())
            .with_probe_memo(memo.clone())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            2,
            "first pass probes both"
        );
        assert_eq!(memo.len(), 1, "the failure is remembered");

        // Second pass: `good` is skipped by its stored (mtime, size); `bad` is
        // skipped by the memo. Neither reaches the prober.
        FsScanner::new(prober.clone())
            .with_probe_memo(memo.clone())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            2,
            "second pass must probe NOTHING; without the memo `bad` is re-read forever"
        );
    }

    /// Replacing the broken file makes it probe again — a memo that outlived
    /// the damage would be worse than the repetition it replaces.
    #[tokio::test]
    async fn replacing_a_broken_file_clears_its_memo() {
        let td = TempDir::new().unwrap();
        let memo_dir = TempDir::new().unwrap();
        touch(td.path(), "bad.mkv").await;
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            force_fail_for: Some("bad".into()),
            genre: None,
            ..Default::default()
        };
        let memo = Arc::new(crate::probe_memo::ProbeMemo::load(
            memo_dir.path().join("probe_failures.json"),
            7,
        ));
        let store = MemStore::default();
        FsScanner::new(prober.clone())
            .with_probe_memo(memo.clone())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(prober.calls.load(Ordering::SeqCst), 1);

        // Rewrite it: same name, different size, so the signature moves.
        write_file(td.path(), "bad.mkv", b"now with actual content").await;
        FsScanner::new(prober.clone())
            .with_probe_memo(memo.clone())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            2,
            "a replaced file must be probed again"
        );
    }

    /// `--force` means every file, including the ones already known broken.
    #[tokio::test]
    async fn force_overrides_the_probe_memo() {
        let td = TempDir::new().unwrap();
        let memo_dir = TempDir::new().unwrap();
        touch(td.path(), "bad.mkv").await;
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            force_fail_for: Some("bad".into()),
            genre: None,
            ..Default::default()
        };
        let memo = Arc::new(crate::probe_memo::ProbeMemo::load(
            memo_dir.path().join("probe_failures.json"),
            7,
        ));
        let store = MemStore::default();
        FsScanner::new(prober.clone())
            .with_probe_memo(memo.clone())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        FsScanner::new(prober.clone())
            .with_probe_memo(memo.clone())
            .with_force(true)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            2,
            "--force must re-probe a memoised file"
        );
    }

    /// AppleDouble sidecars carry the original's extension, so an
    /// extension-only walk picks them up — and ffmpeg rejects them with the
    /// same error a genuinely corrupt file gets, making real damage
    /// indistinguishable from filesystem litter in the log.
    #[tokio::test]
    async fn the_walk_skips_applestyle_sidecars_and_hidden_files() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "real.mkv").await;
        touch(td.path(), "._real.mkv").await;
        touch(td.path(), ".hidden.mkv").await;
        let prober = FakeProber {
            calls: Arc::new(AtomicUsize::new(0)),
            ..Default::default()
        };
        let items = FsScanner::new(prober.clone())
            .scan(td.path())
            .await
            .unwrap();
        assert_eq!(items.len(), 1, "only the real file is catalogued");
        assert_eq!(items[0].title, "real");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            1,
            "the sidecars must not even reach the prober"
        );
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

    fn scan_counter(snap: &pharos_test_metrics::CumulativeSnapshotter, result: &str) -> u64 {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| match v {
                DebugValue::Counter(n)
                    if ck.key().name() == "pharos_scan_items_total"
                        && ck
                            .key()
                            .labels()
                            .any(|l| l.key() == "result" && l.value() == result) =>
                {
                    Some(n)
                }
                _ => None,
            })
            .unwrap_or(0)
    }

    fn series_metric(
        snap: &pharos_test_metrics::CumulativeSnapshotter,
        name: &str,
        labels: &[(&str, &str)],
    ) -> Option<f64> {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let key = ck.key();
                let hit = key.name() == name
                    && labels
                        .iter()
                        .all(|(k, want)| key.labels().any(|l| l.key() == *k && l.value() == *want));
                match v {
                    DebugValue::Counter(n) if hit => Some(n as f64),
                    DebugValue::Gauge(g) if hit => Some(g.into_inner()),
                    _ => None,
                }
            })
    }

    async fn write_show_b(td: &Path) {
        write_file(td, "Show B/Season 1/Show B (1971) - S01E01.mkv", b"a").await;
        write_file(td, "Show B/Season 1/Show B (1996) - S01E01.mkv", b"b").await;
        write_file(td, "Show B/Season 1/Show B (1996) - S01E02.mkv", b"c").await;
    }

    fn keys_of(store: &MemStore) -> Vec<(String, Option<String>, Option<u32>)> {
        let mut out: Vec<_> = store
            .inner
            .lock()
            .unwrap()
            .values()
            .filter_map(|i| {
                let s = i.series.as_ref()?;
                Some((
                    i.path.file_name()?.to_string_lossy().into_owned(),
                    s.series_key.as_ref().map(|k| {
                        k.rsplit_once('\u{1f}')
                            .map_or_else(|| "folder".to_string(), |(_, y)| y.to_string())
                    }),
                    s.series_year,
                ))
            })
            .collect();
        out.sort();
        out
    }

    /// V893: a folder whose files state two show years is two shows; in
    /// apply mode the scan gives the smaller group its own key and remembers
    /// which group keeps the folder's.
    #[tokio::test]
    async fn an_applied_scan_splits_a_folder_stating_two_show_years() {
        use crate::series_split::RepairMode;
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let td = TempDir::new().unwrap();
        write_show_b(td.path()).await;
        let store = MemStore::default();
        let scanner =
            FsScanner::new(FakeProber::default()).with_series_identity_repair(RepairMode::Apply);
        scanner.scan_into(td.path(), &store).await.unwrap();

        assert_eq!(
            keys_of(&store),
            vec![
                (
                    "Show B (1971) - S01E01.mkv".into(),
                    Some("1971".into()),
                    Some(1971)
                ),
                ("Show B (1996) - S01E01.mkv".into(), None, Some(1996)),
                ("Show B (1996) - S01E02.mkv".into(), None, Some(1996)),
            ]
        );
        let owners = store.overrides.lock().unwrap().clone();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].target_year, Some(1996));
        // V940: only the row whose key moved is marked for a rebuild.
        let marked = store.reconcile_marked.lock().unwrap().clone();
        assert_eq!(marked.len(), 1, "{marked:?}");
        assert!(marked[0].ends_with("Show B (1971) - S01E01.mkv"));
        assert_eq!(
            series_metric(
                &snapshotter,
                "pharos_series_reconcile_marked_total",
                &[("reason", "key_changed")]
            ),
            Some(1.0)
        );
        assert_eq!(
            series_metric(
                &snapshotter,
                "pharos_series_folder_split_total",
                &[("reason", "stated_years")]
            ),
            Some(1.0)
        );
        assert_eq!(
            series_metric(
                &snapshotter,
                "pharos_series_folder_split_total",
                &[("reason", "episode_titles")]
            ),
            Some(0.0)
        );
        assert_eq!(
            series_metric(
                &snapshotter,
                "pharos_series_identity_repair_rows",
                &[("change", "series_key"), ("mode", "apply")]
            ),
            Some(3.0)
        );

        // A second pass changes nothing and keeps the remembered owner.
        let before = keys_of(&store);
        scanner.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(keys_of(&store), before);
        assert_eq!(store.overrides.lock().unwrap().len(), 1);
        assert_eq!(store.reconcile_marked.lock().unwrap().len(), 1);
    }

    /// V894: a scan pass splits a folder before folding its copies, so
    /// under `apply` two unyeared copies of one number in a split folder
    /// stay two rows; a dry run folds them as before. Both count the copy
    /// under `decided_by="refused_other_series"`.
    #[tokio::test]
    async fn a_scan_keeps_a_split_folders_number_only_copies_apart() {
        use crate::series_split::RepairMode;
        for mode in [RepairMode::Apply, RepairMode::DryRun] {
            let recorder = metrics_util::debugging::DebuggingRecorder::new();
            let snapshotter = pharos_test_metrics::snapshotter(&recorder);
            let _guard = metrics::set_default_local_recorder(&recorder);
            let td = TempDir::new().unwrap();
            write_show_b(td.path()).await;
            write_file(td.path(), "Show B/Season 1/Show B - S01E03.mkv", b"d").await;
            write_file(td.path(), "Show B/Season 01/Show B - S01E03.avi", b"e").await;
            let store = MemStore::default();
            let scanner = FsScanner::new(FakeProber::default()).with_series_identity_repair(mode);
            scanner.scan_into(td.path(), &store).await.unwrap();

            let items = store.list().await.unwrap();
            let e03: Vec<_> = items
                .iter()
                .filter(|i| {
                    i.path.to_string_lossy().ends_with("S01E03.mkv")
                        || i.path.to_string_lossy().ends_with("S01E03.avi")
                })
                .collect();
            let rows = if mode == RepairMode::Apply { 2 } else { 1 };
            assert_eq!(e03.len(), rows, "{mode:?}: {items:?}");
            let versions: usize = e03.iter().map(|i| i.probe.alternate_sources.len()).sum();
            assert_eq!(versions, 2 - rows, "{mode:?}");
            assert_eq!(
                series_metric(
                    &snapshotter,
                    "pharos_scan_copy_primaries",
                    &[("decided_by", "refused_other_series")]
                ),
                Some(1.0),
                "{mode:?}"
            );
        }
    }

    /// B981: rows migration 0073 backfilled to their folder's key, whose
    /// files now state one show year, get the year written under `apply`
    /// and are not marked for a rebuild: their key did not move.
    #[tokio::test]
    async fn an_applied_year_only_change_marks_nothing_for_a_rebuild() {
        use crate::series_split::RepairMode;
        let td = TempDir::new().unwrap();
        write_file(
            td.path(),
            "Forever (US)/Season 1/Forever (1996) - S01E01.mkv",
            b"a",
        )
        .await;
        write_file(
            td.path(),
            "Forever (US)/Season 1/Forever (1996) - S01E02.mkv",
            b"b",
        )
        .await;
        let store = MemStore::default();
        FsScanner::new(FakeProber::default())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        for item in store.inner.lock().unwrap().values_mut() {
            let s = item.series.as_mut().unwrap();
            s.series_key = s.series_folder.clone();
            s.series_year = None;
        }

        FsScanner::new(FakeProber::default())
            .with_series_identity_repair(RepairMode::Apply)
            .scan_into(td.path(), &store)
            .await
            .unwrap();

        let years: Vec<_> = keys_of(&store).into_iter().map(|(_, _, y)| y).collect();
        assert_eq!(years, [Some(1996), Some(1996)]);
        assert!(store.reconcile_marked.lock().unwrap().is_empty());
    }

    /// V896 — a hand fix is the admin's decision, not the scanner's, so a dry
    /// run writes it: a file an admin row moved takes that row's key and is
    /// marked for a rebuild, while the scanner's own split stays held.
    #[tokio::test]
    async fn a_dry_run_scan_writes_an_admins_move_but_not_its_own_split() {
        let td = TempDir::new().unwrap();
        write_show_b(td.path()).await;
        let folder = td.path().join("Show B").to_string_lossy().into_owned();
        let moved = td
            .path()
            .join("Show B/Season 1/Show B (1996) - S01E02.mkv")
            .to_string_lossy()
            .into_owned();
        let store = MemStore::default();
        store
            .overrides
            .lock()
            .unwrap()
            .push(pharos_core::SeriesOverride {
                id: 1,
                series_folder: folder.clone(),
                path: Some(moved.clone()),
                target_key: "/tv/Elsewhere".into(),
                target_name: Some("Elsewhere".into()),
                target_year: Some(1996),
                source: pharos_core::OverrideSource::Admin,
                created_by: Some("ali".into()),
                created_at: 1,
            });
        let scanner = FsScanner::new(FakeProber::default());
        scanner.scan_into(td.path(), &store).await.unwrap();
        // A second pass finds nothing more to write.
        scanner.scan_into(td.path(), &store).await.unwrap();

        let key_of = |name: &str| {
            store
                .inner
                .lock()
                .unwrap()
                .values()
                .find(|i| i.path.to_string_lossy().ends_with(name))
                .and_then(|i| i.series.clone())
                .map(|s| (s.series_key, s.series_year))
        };
        assert_eq!(
            key_of("(1996) - S01E02.mkv"),
            Some((Some("/tv/Elsewhere".into()), Some(1996)))
        );
        for unsplit in ["(1971) - S01E01.mkv", "(1996) - S01E01.mkv"] {
            assert_eq!(key_of(unsplit), Some((None, None)), "{unsplit}");
        }
        assert_eq!(*store.reconcile_marked.lock().unwrap(), [moved]);
        assert_eq!(
            store.overrides.lock().unwrap().len(),
            1,
            "no owner row in a dry run"
        );
    }

    /// Spec 033 §7 — a hand fix re-plans just its folder from the stored
    /// rows: the admin's move is written (in a dry run too) and returned as
    /// moved, while a neighbouring folder sharing its name's prefix is left
    /// alone.
    #[tokio::test]
    async fn a_hand_fix_reconciles_only_its_own_folder() {
        use crate::series_split::{self as split, RepairMode};
        let td = TempDir::new().unwrap();
        write_show_b(td.path()).await;
        write_file(
            td.path(),
            "Show B 2/Season 1/Show B 2 (1996) - S01E01.mkv",
            b"x",
        )
        .await;
        write_file(
            td.path(),
            "Show B 2/Season 1/Show B 2 (1971) - S01E01.mkv",
            b"y",
        )
        .await;
        let store = MemStore::default();
        FsScanner::new(FakeProber::default())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        let folder = td.path().join("Show B").to_string_lossy().into_owned();
        let moved = td
            .path()
            .join("Show B/Season 1/Show B (1996) - S01E02.mkv")
            .to_string_lossy()
            .into_owned();
        store
            .overrides
            .lock()
            .unwrap()
            .push(pharos_core::SeriesOverride {
                id: 1,
                series_folder: folder.clone(),
                path: Some(moved.clone()),
                target_key: "/tv/Elsewhere".into(),
                target_name: None,
                target_year: Some(1996),
                source: pharos_core::OverrideSource::Admin,
                created_by: Some("ali".into()),
                created_at: 1,
            });
        let paths: Vec<PathBuf> =
            pharos_core::SeriesIdentityStore::series_identity_rows(&store, &folder)
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.series_folder.as_deref() == Some(folder.as_str()))
                .map(|r| PathBuf::from(r.path))
                .collect();
        let folders = split::folders_of(&paths);
        let ctx = split::context(&store, &folders).await.unwrap();
        let plan = split::plan(&folders, &ctx, 2);

        let got = split::reconcile_folder(&store, &plan, &folder, RepairMode::DryRun, 3)
            .await
            .unwrap();

        assert_eq!(got, std::slice::from_ref(&moved));
        assert_eq!(*store.reconcile_marked.lock().unwrap(), [moved]);
        assert!(keys_of(&store)
            .iter()
            .filter(|(name, ..)| name.starts_with("Show B 2 "))
            .all(|(_, k, y)| k.is_none() && y.is_none()));
    }

    /// Spec 033 §6.2: the default dry run counts the split it would make and
    /// writes nothing.
    #[tokio::test]
    async fn a_dry_run_scan_counts_the_split_and_writes_nothing() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let td = TempDir::new().unwrap();
        write_show_b(td.path()).await;
        let store = MemStore::default();
        FsScanner::new(FakeProber::default())
            .scan_into(td.path(), &store)
            .await
            .unwrap();

        assert!(keys_of(&store)
            .iter()
            .all(|(_, k, y)| k.is_none() && y.is_none()));
        assert!(store.overrides.lock().unwrap().is_empty());
        assert_eq!(
            series_metric(
                &snapshotter,
                "pharos_series_folder_split_total",
                &[("reason", "stated_years")]
            ),
            Some(1.0)
        );
        assert_eq!(
            series_metric(
                &snapshotter,
                "pharos_series_identity_repair_rows",
                &[("change", "series_key"), ("mode", "dry_run")]
            ),
            Some(3.0)
        );
    }

    #[tokio::test]
    async fn a_scan_records_its_duration_and_what_it_found() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.mkv", b"aaaa").await;
        write_file(td.path(), "b.mkv", b"bbbb").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());

        s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(scan_counter(&snapshotter, "added"), 2);
        assert_eq!(scan_counter(&snapshotter, "skipped"), 0);

        s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(scan_counter(&snapshotter, "added"), 2, "no new files");
        assert_eq!(scan_counter(&snapshotter, "skipped"), 2);

        let runs =
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(ck, _, _, v)| match v {
                    DebugValue::Histogram(h)
                        if ck.key().name() == "pharos_scan_duration_seconds"
                            && ck
                                .key()
                                .labels()
                                .any(|l| l.key() == "outcome" && l.value() == "ok") =>
                    {
                        Some(h.len())
                    }
                    _ => None,
                });
        assert_eq!(runs, Some(2), "one duration observation per scan");
    }

    /// A slow scan must be attributable to a phase from the trace alone.
    #[tokio::test]
    async fn a_scan_opens_a_span_per_phase() {
        let (spans, _guard) = pharos_test_metrics::spans::capture();
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.mkv", b"aaaa").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());

        s.scan_into(td.path(), &store).await.unwrap();

        for phase in [
            "scan.walk",
            "scan.stat_skip",
            "scan.probe_write",
            "scan.sweep",
        ] {
            assert!(
                spans.contains(phase),
                "missing span {phase}; got {:?}",
                spans.names()
            );
        }
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
    async fn scan_progress_reports_from_zero_to_complete() {
        use std::sync::Mutex;
        // A first scan (all four files probed) and a second scan (all four
        // skipped) must both drive the progress sink from 0 to processed==total,
        // monotonically, with a constant total equal to the group count.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.mkv", b"aaaa").await;
        write_file(td.path(), "b.mkv", b"bbbb").await;
        write_file(td.path(), "show/s1/ep1.mkv", b"cccc").await;
        write_file(td.path(), "show/s1/ep2.mkv", b"dddd").await;
        let store = MemStore::default();

        // Pass 1 — every file is probed (phase 3 drives progress).
        let seen1: Arc<Mutex<Vec<ScanProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let sink1 = seen1.clone();
        let out1 = FsScanner::new(FakeProber::default())
            .with_progress(Arc::new(move |p: ScanProgress| {
                sink1.lock().unwrap().push(p)
            }))
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(out1.added.len(), 4, "first scan adds all four");
        assert_scan_progress_completes(&seen1.lock().unwrap(), 4);

        // Pass 2 — nothing changed on disk; every file skips in phase 1, yet
        // progress must still reach 100% (the skip path drives progress too).
        let seen2: Arc<Mutex<Vec<ScanProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let sink2 = seen2.clone();
        let out2 = FsScanner::new(FakeProber::default())
            .with_progress(Arc::new(move |p: ScanProgress| {
                sink2.lock().unwrap().push(p)
            }))
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(out2.skipped, 4, "second scan skips all four");
        assert_scan_progress_completes(&seen2.lock().unwrap(), 4);
    }

    fn assert_scan_progress_completes(seen: &[ScanProgress], total: u64) {
        assert!(!seen.is_empty(), "progress sink must fire at least once");
        assert!(
            seen.iter().all(|p| p.total == total),
            "total constant = {total}: {seen:?}"
        );
        for w in seen.windows(2) {
            assert!(
                w[1].processed >= w[0].processed,
                "processed must not go backwards: {seen:?}"
            );
        }
        assert_eq!(seen[0].processed, 0, "first callback is 0%: {seen:?}");
        let last = seen[seen.len() - 1];
        assert_eq!(
            last.processed, total,
            "final callback is complete: {seen:?}"
        );
        assert_eq!(last.percent(), 100.0, "final percent is 100: {seen:?}");
    }

    #[tokio::test]
    async fn cancelled_scan_skips_sweep_and_prunes_nothing() {
        use std::sync::atomic::AtomicBool;
        // A pre-cancelled run must NOT delete rows for files it never got to
        // visit — otherwise cancelling a scan would nuke the library. The sweep
        // is skipped exactly like an incomplete walk.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.mkv", b"aaaa").await;
        write_file(td.path(), "b.mkv", b"bbbb").await;
        let store = MemStore::default();
        assert_eq!(
            FsScanner::new(FakeProber::default())
                .scan_into(td.path(), &store)
                .await
                .unwrap()
                .added
                .len(),
            2
        );

        // Remove a file on disk: a normal scan would sweep its row. A cancelled
        // one must not.
        tokio::fs::remove_file(td.path().join("a.mkv"))
            .await
            .unwrap();
        let flag = Arc::new(AtomicBool::new(true));
        let out = FsScanner::new(FakeProber::default())
            .with_cancel(flag)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert!(
            out.removed.is_empty(),
            "cancelled scan must not sweep: {out:?}"
        );
        assert_eq!(
            store.list().await.unwrap().len(),
            2,
            "both rows survive a cancelled scan even though a.mkv is gone"
        );
    }

    async fn many_files(n: usize) -> TempDir {
        let td = TempDir::new().unwrap();
        for i in 0..n {
            write_file(td.path(), &format!("m{i:03}.mkv"), b"x").await;
        }
        td
    }

    #[tokio::test]
    async fn probed_items_are_written_in_batches_of_the_configured_size() {
        let td = many_files(25).await;
        let store = MemStore::default();
        let out = FsScanner::new(FakeProber::default())
            .with_probe_concurrency(1)
            .with_put_batch_size(10)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(out.added.len(), 25);
        assert_eq!(store.list().await.unwrap().len(), 25);
        assert_eq!(*store.put_batches.lock().unwrap(), [10, 10, 5]);
    }

    #[tokio::test]
    async fn batch_size_one_writes_each_item_alone() {
        let td = many_files(4).await;
        let store = MemStore::default();
        FsScanner::new(FakeProber::default())
            .with_put_batch_size(1)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(*store.put_batches.lock().unwrap(), [1, 1, 1, 1]);
    }

    #[tokio::test]
    async fn a_partial_batch_is_written_once_its_interval_elapses() {
        // Slow disk: 10 files at ~150 ms each, batches far bigger than the
        // scan, a 400 ms flush interval (about 3 files per flush, so a stall of
        // a few hundred ms on a busy runner cannot merge them into one).
        let td = many_files(10).await;
        let store = MemStore::default();
        let prober = FakeProber {
            delay: Some(std::time::Duration::from_millis(150)),
            ..Default::default()
        };
        FsScanner::new(prober)
            .with_probe_concurrency(1)
            .with_put_batch_size(1_000)
            .with_put_flush_interval(std::time::Duration::from_millis(400))
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        let batches = store.put_batches.lock().unwrap().clone();
        assert_eq!(batches.iter().sum::<usize>(), 10, "{batches:?}");
        assert!(
            batches.len() >= 2 && batches.iter().all(|n| *n < 10),
            "expected several interval flushes, got {batches:?}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_scan_still_writes_what_it_already_probed() {
        use std::sync::atomic::AtomicBool;
        let td = many_files(10).await;
        let store = MemStore::default();
        let flag = Arc::new(AtomicBool::new(false));
        let prober = FakeProber {
            cancel_during: Some((4, flag.clone())),
            ..Default::default()
        };
        FsScanner::new(prober)
            .with_probe_concurrency(1)
            .with_put_batch_size(100)
            .with_cancel(flag)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        // Probes 1-3 were buffered when the 4th raised the flag; they must
        // reach the store, and the follow-up writes must have run for them.
        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 3, "probed-before-cancel items are kept");
        for r in &rows {
            assert!(
                store.scan_state(r.id).await.unwrap().is_some(),
                "mark_seen ran for {}",
                r.id
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_row_does_not_strand_the_rest_of_its_batch() {
        let td = many_files(6).await;
        let store = MemStore::default();
        let bad = stable_id(&td.path().join("m002.mkv"));
        store.reject_ids.lock().unwrap().insert(bad);
        let err = FsScanner::new(FakeProber::default())
            .with_probe_concurrency(1)
            .with_put_batch_size(6)
            .scan_into(td.path(), &store)
            .await;
        assert!(err.is_err(), "the rejected row still fails the scan");
        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 5, "every other row landed");
        for r in &rows {
            assert!(
                store.scan_state(r.id).await.unwrap().is_some(),
                "follow-ups (mark_seen) ran for {}",
                r.id
            );
        }
    }

    fn rewriting(needle: &str) -> FakeProber {
        FakeProber {
            rewrite_during: Some((needle.into(), b"changed while probing".to_vec())),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_probe_of_a_file_that_changed_while_buffered_is_not_written() {
        let td = many_files(6).await;
        let store = MemStore::default();
        let out = FsScanner::new(rewriting("m002"))
            .with_probe_concurrency(1)
            .with_put_batch_size(6)
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        let ids: HashSet<_> = store.list().await.unwrap().iter().map(|i| i.id).collect();
        assert!(!ids.contains(&stable_id(&td.path().join("m002.mkv"))));
        assert_eq!((ids.len(), out.added.len(), out.skipped), (5, 5, 1));

        // The next scan sees the file as it is now and picks it up.
        let again = FsScanner::new(FakeProber::default())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(again.added.len(), 1);
        assert_eq!(store.list().await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn a_stale_probe_leaves_the_existing_row_alone_and_unswept() {
        let td = many_files(6).await;
        let store = MemStore::default();
        FsScanner::new(FakeProber::default())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        let id = stable_id(&td.path().join("m002.mkv"));
        let before = store.get(id).await.unwrap();
        write_file(td.path(), "m002.mkv", b"a different size").await;

        let out = FsScanner::new(rewriting("m002"))
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert!(
            out.removed.is_empty(),
            "the sweep must not drop it: {out:?}"
        );
        assert_eq!(
            store.get(id).await.unwrap(),
            before,
            "stale probe not stored"
        );
        // And it is re-probed next time rather than treated as up to date.
        let next = FsScanner::new(FakeProber::default())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        assert_eq!(next.updated, vec![id]);
    }

    #[tokio::test]
    async fn progress_never_runs_ahead_of_what_is_written() {
        let td = many_files(12).await;
        let store = Arc::new(MemStore::default());
        let reports = Arc::new(Mutex::new(Vec::new()));
        let (s, r) = (store.clone(), reports.clone());
        FsScanner::new(FakeProber::default())
            .with_probe_concurrency(1)
            .with_put_batch_size(5)
            .with_progress(Arc::new(move |p: ScanProgress| {
                let written = s.inner.lock().unwrap().len() as u64;
                r.lock().unwrap().push((p.processed, written));
            }))
            .scan_into(td.path(), &*store)
            .await
            .unwrap();
        let reports = reports.lock().unwrap();
        for (processed, written) in reports.iter() {
            assert!(
                processed <= written,
                "reported {processed} done with only {written} rows written: {reports:?}"
            );
        }
        assert_eq!(reports.last().map(|r| r.0), Some(12));
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

    /// 005-kindle-conversion — a book's freshness is governed by the BOOK
    /// schema version, not the probe one.
    ///
    /// Both directions matter and the second is the one that shipped broken:
    /// a book row carrying the current PROBE version must still be re-read
    /// (that is exactly the state the deployed library was in — 75 Kindle rows
    /// at probe v6, skipped forever, feature inert), and once re-read at the
    /// book version it must SETTLE rather than re-read on every scan.
    #[tokio::test]
    async fn a_books_staleness_is_governed_by_the_book_schema_version() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "book.epub", b"not really an epub").await;
        let prober = FakeProber::default();
        let store = MemStore::default();
        let s = FsScanner::new(prober.clone());

        assert_eq!(s.scan_into(td.path(), &store).await.unwrap().added.len(), 1);
        let id = stable_id(&td.path().join("book.epub"));

        // A book is stamped with the BOOK version, never the probe version —
        // otherwise the two pipelines are welded together again.
        assert_eq!(
            store.states.lock().unwrap()[&id].probe_schema_version,
            pharos_core::BOOK_SCHEMA_VERSION,
        );

        // Settles: unchanged on disk and at the current book version → skipped.
        assert_eq!(
            s.scan_into(td.path(), &store).await.unwrap().skipped,
            1,
            "a book at the current book version must skip, not re-read forever"
        );

        // The deployed library's exact state: a book row left carrying the
        // PROBE version. It must NOT be skipped.
        store
            .states
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .probe_schema_version = pharos_core::PROBE_SCHEMA_VERSION;
        assert_eq!(
            s.scan_into(td.path(), &store).await.unwrap().skipped,
            0,
            "a book stamped with the probe version is stale and must be re-read \
             — skipping it is how Kindle conversion shipped to a library where \
             nothing changed"
        );

        // A book is still never handed to the prober (V12/FR-001).
        assert_eq!(prober.calls.load(Ordering::SeqCst), 0);
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

    /// V136 — a scan of a real root must not sweep URL-backed rows.
    ///
    /// `sweep_unseen` deletes every row under the scanned root that this pass
    /// did not see. A synthetic `ytdlp://` path is not under any filesystem
    /// root, so it survives — but that is a PROPERTY of the arrangement, not an
    /// accident, and nothing else in the codebase pins it.
    ///
    /// Getting it wrong is silent and total. Park remote items beneath a real
    /// root and the walk succeeds, finds none of them, and deletes the lot —
    /// and B98's blast-radius guard does not save you, because it needs both
    /// >=100 deletions AND >25% of the library. A household's 40 web videos
    /// vanish without so much as a warning line.
    #[tokio::test]
    async fn a_scan_never_sweeps_a_url_backed_row() {
        let root = TempDir::new().unwrap();
        write_file(root.path(), "local.mkv", b"local").await;

        let prober = FakeProber::default();
        let s = FsScanner::new(prober.clone());
        let store = MemStore::default();
        s.scan_into(root.path(), &store).await.unwrap();

        // A URL-backed item, written the way the ingestion endpoint writes it.
        let remote_path = pharos_core::RemoteRef::new("youtube", "dQw4w9WgXcQ")
            .expect("valid ref")
            .to_synthetic_path();
        let remote_id = stable_id(&remote_path);
        store
            .put(MediaItem {
                id: remote_id,
                path: remote_path,
                title: "Never Gonna Give You Up".into(),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(store.list().await.unwrap().len(), 2);

        // A full, CLEAN rescan — walk_errors == 0, so the sweep really runs.
        // (If it were skipped the test would pass for the wrong reason, which
        // is exactly how this class of bug hides.)
        let local_id = stable_id(&root.path().join("local.mkv"));
        tokio::fs::remove_file(root.path().join("local.mkv"))
            .await
            .unwrap();
        let outcome = s.scan_into(root.path(), &store).await.unwrap();

        assert_eq!(
            outcome.removed,
            vec![local_id],
            "the sweep must have actually run and taken the deleted local file"
        );
        assert!(
            store.get(remote_id).await.is_ok(),
            "a URL-backed row must survive a scan of a filesystem root"
        );

        // And the hazard is REAL, not hypothetical: the same row parked under
        // the scanned root is destroyed by the very next pass. This half is
        // what stops the assertion above from passing vacuously — it is only
        // meaningful because the alternative arrangement demonstrably loses
        // data.
        let parked = root.path().join("ytdlp-youtube-dQw4w9WgXcQ");
        let parked_id = stable_id(&parked);
        store
            .put(MediaItem {
                id: parked_id,
                path: parked,
                title: "parked the wrong way".into(),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
        let outcome = s.scan_into(root.path(), &store).await.unwrap();
        assert!(
            outcome.removed.contains(&parked_id),
            "parking a synthetic row under a real root loses it on the next scan — \
             which is why the library root is a prefix no walk can reach (V136)"
        );
        assert!(
            store.get(remote_id).await.is_ok(),
            "while the correctly-placed row is still there"
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

    /// B587 — Kodi's `1x16` names an episode; a frame size, a title's `2x4`
    /// or a film's `10x10` does not.
    #[test]
    fn an_nxnn_token_names_an_episode() {
        assert!(has_nxnn_token(
            "Star Trek The Next Generation - 1x16 - Too Short a Season.avi"
        ));
        assert!(has_nxnn_token(
            "Star Trek The Next Generation - 1x01x02 - Encounter at Farpoint.avi"
        ));
        assert!(has_nxnn_token("show.1x105.mkv"));
        assert!(!has_nxnn_token("Movie 1920x1080.mkv"));
        assert!(!has_nxnn_token("The 2x4 Club (2001).mkv"));
        assert!(!has_nxnn_token("Movie 1x16mm.mkv"));
        assert!(!has_nxnn_token("Box1x16.mkv"));
        assert!(!has_nxnn_token("Movie (2024).mkv"));
        assert!(!has_nxnn_token("Ocean 10x10.mkv"));
        assert!(has_nxnn_token("Long Show - 10x05 - Title.mkv"));
        assert!(!has_nxnn_token("Zé1x16.mkv"));
        assert!(is_episode_path(Path::new(
            "/media/TV/Star Trek The Next Generation/Star Trek The Next Generation - 1x16 - Too Short a Season.avi"
        )));
        let info = parse_series_info(Path::new(
            "/media/TV/Star Trek The Next Generation/Star Trek The Next Generation - 1x16 - Too Short a Season.avi",
        ))
        .expect("series info");
        assert_eq!(info.series_name, "Star Trek The Next Generation");
        assert_eq!(
            (info.season_number, info.episode_number),
            (Some(1), Some(16))
        );
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

    /// B1008 / V967 — a half-numbered recap (`- 14.5`) is no episode 14, or
    /// it folds in as a copy of 14; a release tag after `S01E03.` stays read.
    #[test]
    fn a_half_numbered_recap_is_not_its_whole_episode() {
        assert_eq!(parse_episode_from_name("SAO II - 14.5 L@mBerT.mp4"), None);
        assert_eq!(parse_episode_from_name("Show - 07.5.mkv"), None);
        assert_eq!(
            parse_episode_from_name("SAO II - 14 L@mBerT.mp4"),
            Some((None, 14))
        );
        assert_eq!(
            parse_episode_from_name("Person.of.Interest.S01E03.720p.HDTV.mkv"),
            Some((Some(1), 3))
        );
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

    #[test]
    fn a_release_folder_in_the_library_root_names_its_show() {
        let p = Path::new(
            "/tv/Person.of.Interest.S01E03.720p.HDTV.x264-IMMERSE/Person.of.Interest.S01E03.720p.HDTV.x264-IMMERSE.mkv",
        );
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Person of Interest");
        assert_eq!(
            info.series_folder.as_deref(),
            Some("/tv/Person of Interest")
        );
        assert_eq!(info.season_number, Some(1));
        assert_eq!(info.episode_number, Some(3));

        let other = parse_series_info(Path::new(
            "/tv/Person.of.Interest.S01E04.720p.HDTV.x264-LOL/poi.104.mkv",
        ))
        .expect("series info");
        assert_eq!(other.series_key(), info.series_key());
    }

    #[test]
    fn a_release_folder_with_a_bracketed_tag_names_its_show() {
        let p = Path::new(
            "/tv/The.Vampire.Diaries.S04E21.720p.HDTV.X264-DIMENSION [PublicHD]/The.Vampire.Diaries.S04E21.720p.HDTV.X264-DIMENSION.mkv",
        );
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "The Vampire Diaries");
        assert_eq!(
            info.series_folder.as_deref(),
            Some("/tv/The Vampire Diaries")
        );
        assert_eq!(info.season_number, Some(4));
        assert_eq!(info.episode_number, Some(21));
    }

    #[test]
    fn a_release_folder_drops_its_tags_and_keeps_its_year() {
        let p =
            Path::new("/tv/The.Vampire.Diaries.S04E21.720p.HDTV.X264-DIMENSION [PublicHD]/x.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "The Vampire Diaries");
        assert_eq!(info.season_number, Some(4));

        let pack = parse_series_info(Path::new(
            "/tv/Doctor.Who.2005.S02.1080p.BluRay.x264-GRP/Doctor.Who.2005.S02E01.mkv",
        ))
        .expect("series info");
        assert_eq!(pack.series_name, "Doctor Who");
        assert_eq!(pack.series_year, Some(2005));
        assert_eq!(pack.season_number, Some(2));
        assert_eq!(pack.episode_number, Some(1));
    }

    #[test]
    fn a_release_folder_inside_its_show_folder_keeps_the_show_folder() {
        let p = Path::new(
            "/tv/Person of Interest (2011)/Person.of.Interest.S01E03.720p.HDTV.x264-IMMERSE/e.mkv",
        );
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Person of Interest");
        assert_eq!(
            info.series_folder.as_deref(),
            Some("/tv/Person of Interest (2011)")
        );
        assert_eq!(info.series_year, Some(2011));
    }

    #[test]
    fn nested_release_folders_resolve_to_the_show_folder() {
        let p = Path::new("/tv/Show/Show.S01.Complete.720p/Show.S01E01.720p-GRP/x.mkv");
        let info = parse_series_info(p).expect("series info");
        assert_eq!(info.series_name, "Show");
        assert_eq!(info.series_folder.as_deref(), Some("/tv/Show"));

        let rooted = parse_series_info(Path::new(
            "/tv/Doctor.Who.2005.S02.1080p/Doctor.Who.2005.S02E01.720p-GRP/x.mkv",
        ))
        .expect("series info");
        assert_eq!(rooted.series_folder.as_deref(), Some("/tv/Doctor Who"));

        let in_show = parse_series_info(Path::new(
            "/tv/Doctor Who/Doctor.Who.2005.S02E01.720p-GRP/x.mkv",
        ))
        .expect("series info");
        assert_eq!(in_show.series_folder.as_deref(), Some("/tv/Doctor Who"));
        assert_eq!(in_show.series_year, Some(2005));
    }

    #[test]
    fn a_show_folder_with_a_number_is_not_a_release_folder() {
        for show in ["Stargate SG-1", "Show 1920x1080", "Ep 12", "Glee"] {
            assert!(parse_release_folder(show).is_none(), "{show}");
        }
        let info = parse_series_info(Path::new("/tv/Stargate SG-1/Season 02/S02E05.mkv"))
            .expect("series info");
        assert_eq!(info.series_name, "Stargate SG-1");
    }

    // LIB-C11 — two distinct shows sharing a name must yield the SAME
    // series_name but DIFFERENT series_folder + year so their synthesised
    // wire ids diverge and their episodes don't interleave.
    #[test]
    fn same_name_shows_get_distinct_folders_and_years() {
        let p80 = Path::new("/tv/Show A (1961)/Season 01/S01E01.mkv");
        let p14 = Path::new("/tv/Show A (1996)/Season 01/S01E01.mkv");
        let i80 = parse_series_info(p80).expect("series info 1961");
        let i14 = parse_series_info(p14).expect("series info 1996");

        // Display name is identical (the bare show name)…
        assert_eq!(i80.series_name, "Show A");
        assert_eq!(i14.series_name, "Show A");
        // …but the folder identity + parsed year differ.
        assert_eq!(i80.series_folder.as_deref(), Some("/tv/Show A (1961)"));
        assert_eq!(i14.series_folder.as_deref(), Some("/tv/Show A (1996)"));
        assert_eq!(i80.series_year, Some(1961));
        assert_eq!(i14.series_year, Some(1996));
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
        assert_eq!(parse_folder_year("Show A (1961)"), Some(1961));
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
    fn unique_edition_id_suffixes_a_repeated_slug() {
        let alt = |id: &str| AlternateMediaSource {
            id: id.into(),
            path: PathBuf::from("/x"),
            container: None,
            video_codec: None,
            audio_codec: None,
            bitrate_bps: None,
            size_bytes: None,
            duration_ms: None,
            name: None,
            video_3d_format: None,
            probe: None,
            part: None,
        };
        assert_eq!(unique_edition_id(&[], "3D HSBS"), "3d-hsbs");
        let one = [alt("3d-hsbs")];
        assert_eq!(unique_edition_id(&one, "3D.HSBS"), "3d-hsbs-2");
        let two = [alt("3d-hsbs"), alt("3d-hsbs-2")];
        assert_eq!(unique_edition_id(&two, "3D HSBS"), "3d-hsbs-3");
    }

    #[test]
    fn edition_id_slug_is_url_safe() {
        assert_eq!(edition_id_slug("Director's Cut"), "director-s-cut");
        assert_eq!(edition_id_slug("IMAX"), "imax");
        assert_eq!(edition_id_slug("Extended Edition"), "extended-edition");
    }

    #[test]
    fn parse_3d_edition_needs_a_packing_tag() {
        assert_eq!(
            parse_3d_edition("3D.HSBS"),
            Some(Video3DFormat::HalfSideBySide)
        );
        assert_eq!(
            parse_3d_edition("3d ftab"),
            Some(Video3DFormat::FullTopAndBottom)
        );
        assert_eq!(parse_3d_edition("3D.MVC"), Some(Video3DFormat::Mvc));
        // A bare "3D" says nothing about the layout.
        assert_eq!(parse_3d_edition("3D"), None);
        assert_eq!(parse_3d_edition("3D.XYZ"), None);
        assert_eq!(parse_3d_edition("3DHSBS"), None);
        assert_eq!(parse_3d_edition("Extended"), None);
    }

    #[test]
    fn split_edition_tag_recognises_3d_labels() {
        assert_eq!(
            split_edition_tag("Avatar (2009) - 3D.HSBS"),
            Some(("Avatar (2009)", "3D.HSBS"))
        );
        assert_eq!(
            split_edition_tag("Foo S01E01 - 3D HTAB"),
            Some(("Foo S01E01", "3D HTAB"))
        );
        assert_eq!(split_edition_tag("Avatar - 3D"), None);
    }

    #[test]
    fn edition_display_name_normalises_3d_only() {
        assert_eq!(edition_display_name("3d.hsbs"), "3D HSBS");
        assert_eq!(edition_display_name("Director's Cut"), "Director's Cut");
    }

    #[tokio::test]
    async fn a_3d_rip_groups_under_its_2d_movie_with_its_format() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Avatar (2009)/Avatar (2009).mkv").await;
        touch(td.path(), "Avatar (2009)/Avatar (2009) - 3D.HSBS.mkv").await;
        let items = FsScanner::new(FakeProber::default())
            .scan(td.path())
            .await
            .unwrap();
        assert_eq!(items.len(), 1, "3D rip must not be its own item");
        let probe = &items[0].probe;
        assert_eq!(probe.video_3d_format, None);
        assert_eq!(probe.alternate_sources.len(), 1);
        let alt = &probe.alternate_sources[0];
        assert_eq!(alt.id, "3d-hsbs");
        assert_eq!(alt.name.as_deref(), Some("3D HSBS"));
        assert_eq!(alt.video_3d_format, Some(Video3DFormat::HalfSideBySide));
        let own = alt.probe.as_deref().expect("edition keeps its own probe");
        assert_eq!(own.video_3d_format, Some(Video3DFormat::HalfSideBySide));
        assert!(own.alternate_sources.is_empty());
    }

    fn reprobe_counter(snap: &pharos_test_metrics::CumulativeSnapshotter, reason: &str) -> u64 {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| match v {
                DebugValue::Counter(n)
                    if ck.key().name() == "pharos_scan_reprobe_total"
                        && ck
                            .key()
                            .labels()
                            .any(|l| l.key() == "reason" && l.value() == reason) =>
                {
                    Some(n)
                }
                _ => None,
            })
            .unwrap_or(0)
    }

    const FILM: &str = "Film (2010)/Film (2010).mkv";
    const FILM_3D: &str = "Film (2010)/Film (2010) - 3D.HSBS.mkv";

    #[test]
    fn reprobe_reason_labels_are_distinct() {
        let labels: HashSet<&'static str> = ReprobeReason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), ReprobeReason::ALL.len());
    }

    #[tokio::test]
    async fn a_rescan_drops_an_edition_whose_file_was_deleted() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = TempDir::new().unwrap();
        write_file(td.path(), FILM, b"2d").await;
        write_file(td.path(), FILM_3D, b"3dddd").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        s.scan_into(td.path(), &store).await.unwrap();
        let id = stable_id(&td.path().join(FILM));
        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.probe.alternate_sources.len(), 1);
        assert!(
            stored.probe.alternate_sources[0].video_3d_format.is_some(),
            "the 3D edition makes the item 3D"
        );

        tokio::fs::remove_file(td.path().join(FILM_3D))
            .await
            .unwrap();
        let out = s.scan_into(td.path(), &store).await.unwrap();

        let stored = store.get(id).await.unwrap();
        assert!(
            stored.probe.alternate_sources.is_empty(),
            "deleted edition must leave the row: {:?}",
            stored.probe.alternate_sources
        );
        assert_eq!(out.updated, [id], "the primary was re-put, not skipped");
        assert_eq!(reprobe_counter(&snapshotter, "edition_removed"), 1);
    }

    #[tokio::test]
    async fn a_rescan_picks_up_an_edition_added_beside_a_scanned_primary() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = TempDir::new().unwrap();
        write_file(td.path(), FILM, b"2d").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        s.scan_into(td.path(), &store).await.unwrap();
        let id = stable_id(&td.path().join(FILM));
        assert!(store
            .get(id)
            .await
            .unwrap()
            .probe
            .alternate_sources
            .is_empty());

        write_file(td.path(), FILM_3D, b"3dddd").await;
        s.scan_into(td.path(), &store).await.unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.probe.alternate_sources.len(), 1);
        assert_eq!(
            stored.probe.alternate_sources[0].video_3d_format,
            Some(Video3DFormat::HalfSideBySide)
        );
        assert_eq!(reprobe_counter(&snapshotter, "edition_added"), 1);
    }

    #[tokio::test]
    async fn a_rescan_refreshes_an_edition_whose_file_changed_size() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = TempDir::new().unwrap();
        write_file(td.path(), FILM, b"2d").await;
        write_file(td.path(), FILM_3D, b"3dddd").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        s.scan_into(td.path(), &store).await.unwrap();
        let id = stable_id(&td.path().join(FILM));

        write_file(td.path(), FILM_3D, b"3d-re-encoded-bigger").await;
        s.scan_into(td.path(), &store).await.unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(
            stored.probe.alternate_sources[0].size_bytes,
            Some(b"3d-re-encoded-bigger".len() as u64)
        );
        assert_eq!(reprobe_counter(&snapshotter, "edition_changed"), 1);
    }

    #[tokio::test]
    async fn an_unchanged_rescan_of_a_grouped_primary_still_skips() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = TempDir::new().unwrap();
        write_file(td.path(), FILM, b"2d").await;
        write_file(td.path(), FILM_3D, b"3dddd").await;
        let prober = FakeProber::default();
        let store = MemStore::default();
        let s = FsScanner::new(prober.clone());
        s.scan_into(td.path(), &store).await.unwrap();

        let before = prober.calls.load(Ordering::SeqCst);
        let out = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(out.skipped, 1, "nothing changed: the primary skips");
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            before,
            "no extra probe"
        );
        for reason in ReprobeReason::ALL {
            assert_eq!(reprobe_counter(&snapshotter, reason.label()), 0);
        }
    }

    #[tokio::test]
    async fn an_unchanged_file_without_editions_reads_no_stored_editions() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), FILM, b"2d").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        s.scan_into(td.path(), &store).await.unwrap();

        let out = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(out.skipped, 1, "nothing changed: the file skips");
        assert_eq!(
            store.alternate_sources_calls.load(Ordering::SeqCst),
            0,
            "no editions stored or on disk: no extra store read"
        );
    }

    #[tokio::test]
    async fn an_unchanged_grouped_primary_still_reads_its_stored_editions() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), FILM, b"2d").await;
        write_file(td.path(), FILM_3D, b"3dddd").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        s.scan_into(td.path(), &store).await.unwrap();

        s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(store.alternate_sources_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn edition_drift_reports_the_offending_path() {
        let alt = |path: &str, size: Option<u64>| AlternateMediaSource {
            path: path.into(),
            size_bytes: size,
            ..Default::default()
        };
        let on_disk = |path: &str, size: Option<u64>| (PathBuf::from(path), size);
        let same = [alt("/a", Some(1))];
        assert_eq!(edition_drift(&same, &[on_disk("/a", Some(1))]), None);
        assert_eq!(
            edition_drift(&same, &[]),
            Some((ReprobeReason::Removed, "/a".into()))
        );
        assert_eq!(
            edition_drift(&[], &[on_disk("/b", Some(1))]),
            Some((ReprobeReason::Added, "/b".into()))
        );
        assert_eq!(
            edition_drift(&same, &[on_disk("/a", Some(2))]),
            Some((ReprobeReason::Changed, "/a".into()))
        );
        // Unknown on either side is not a change: nothing to compare.
        assert_eq!(edition_drift(&same, &[on_disk("/a", None)]), None);
        assert_eq!(
            edition_drift(&[alt("/a", None)], &[on_disk("/a", Some(2))]),
            None
        );
    }

    #[tokio::test]
    async fn a_3d_episode_groups_under_its_2d_episode() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Foo/Season 01/Foo S01E01.mkv").await;
        touch(td.path(), "Foo/Season 01/Foo S01E01 - 3D.FSBS.mkv").await;
        let items = FsScanner::new(FakeProber::default())
            .scan(td.path())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].probe.alternate_sources[0].video_3d_format,
            Some(Video3DFormat::FullSideBySide)
        );
    }

    fn names(alts: &[(String, PathBuf)]) -> Vec<&str> {
        alts.iter().map(|(label, _)| label.as_str()).collect()
    }

    #[test]
    fn copies_of_an_episode_in_two_season_folders_group_as_one() {
        let a = PathBuf::from("/tv/Black Mirror/Season 1/Black Mirror - S01E02 - Fifteen Million Merits WEBDL-1080p.mkv");
        let b = PathBuf::from("/tv/Black Mirror/Season 01/Black Mirror - S01E02 - Fifteen Million Merits WEBDL-1080p.mkv");
        let groups = group_editions(vec![a.clone(), b.clone()]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, b);
        assert_eq!(groups[0].1, vec![("Season 1".to_string(), a)]);
    }

    #[test]
    fn a_copy_is_labelled_by_what_its_name_adds() {
        let groups = group_editions(vec![
            PathBuf::from("/tv/Abbott/Season 5/Abbott - S05E19 - Trip WEBDL-1080p.mkv"),
            PathBuf::from("/tv/Abbott/Season 5/Abbott - S05E19 - Trip HDTV-1080p Proper.mkv"),
            PathBuf::from("/tv/Show C/Season 01/Show C - S01E01 - 33.avi"),
            PathBuf::from("/tv/Show C/Season 01/Show C - S01E01 - 33.mkv"),
        ]);
        assert_eq!(groups.len(), 2);
        let abbott = groups
            .iter()
            .find(|g| g.0.starts_with("/tv/Abbott"))
            .unwrap();
        assert!(abbott
            .0
            .ends_with("Abbott - S05E19 - Trip HDTV-1080p Proper.mkv"));
        assert_eq!(names(&abbott.1), ["WEBDL-1080p"]);
        let show_c = groups
            .iter()
            .find(|g| g.0.starts_with("/tv/Show C"))
            .unwrap();
        assert_eq!(
            show_c.0.extension().unwrap(),
            "mkv",
            "mkv outranks avi as primary"
        );
        assert_eq!(names(&show_c.1), ["AVI"]);
    }

    #[test]
    fn a_copy_label_names_what_sets_it_apart_not_a_clipped_tail() {
        let label = |a: &str, b: &str| copy_label(Path::new(a), Path::new(b));
        assert_eq!(
            label(
                "/m/Transformers Age of Extinction/Transformers - Age of Extinction (2014) WEBDL-1080p.mkv",
                "/m/Transformers Age of Extinction (2014)/Transformers Age of Extinction (2014) Bluray-1080p.mkv",
            ),
            "Bluray-1080p"
        );
        assert_eq!(
            label(
                "/tv/Grimm/Season 1/Grimm - S01E09 - Of Mouse and Man WEBDL-1080p.mkv",
                "/tv/Grimm/Season 01/Grimm - S01E09 - Of Mouse and Man.mkv",
            ),
            "Season 01",
            "a copy that adds no words is named by its folder"
        );
        assert_eq!(
            label(
                "/m/Music/System of a Down/Hypnotize/01 Attack.mp3",
                "/m/Music/System Of A Down/Hypnotize/01 - Attack.mp3",
            ),
            "System Of A Down",
            "the nearest folder that differs, not the shared album folder"
        );
        assert_eq!(
            label(
                "/m/Music/SOAD/Hypnotize/10 She’s Like Heroin.mp3",
                "/m/Music/Soad/Hypnotize/10 - She's Like Heroin.mp3",
            ),
            "Soad",
            "a curly and a straight apostrophe are one word"
        );
        assert_eq!(
            label(
                "/m/Shawshank (1994)/Shawshank (1994) Bluray-1080p Proper.mkv",
                "/m/Shawshank (1994)/Shawshank (1994) Bluray-1080p.mkv",
            ),
            "Bluray-1080p"
        );
        assert_eq!(label("/m/A/Song.mp3", "/m/A/song.mp3"), "Copy");
    }

    #[test]
    fn different_episodes_shows_and_seasons_stay_apart() {
        let groups = group_editions(vec![
            PathBuf::from("/tv/Foo/Season 01/Foo S01E01.mkv"),
            PathBuf::from("/tv/Foo/Season 01/Foo S01E02.mkv"),
            PathBuf::from("/tv/Foo/Season 02/Foo S02E01.mkv"),
            PathBuf::from("/tv/Bar/Season 01/Bar S01E01.mkv"),
            PathBuf::from("/films/Heat (1995).mkv"),
        ]);
        assert_eq!(groups.len(), 5);
        assert!(groups.iter().all(|(_, alts)| alts.is_empty()));
    }

    fn group_of<'a>(
        groups: &'a [(PathBuf, Vec<(String, PathBuf)>)],
        primary: &Path,
    ) -> Option<&'a [(String, PathBuf)]> {
        groups
            .iter()
            .find(|(p, _)| p == primary)
            .map(|(_, alts)| alts.as_slice())
    }

    fn alt_paths(alts: &[(String, PathBuf)]) -> Vec<&Path> {
        alts.iter().map(|(_, p)| p.as_path()).collect()
    }

    /// V844: a double episode is a copy of each episode it covers. With a
    /// single for E01 and none for E02, it is a version of E01 and stays the
    /// item that holds E02.
    #[test]
    fn a_double_episode_is_a_version_of_each_episode_it_covers() {
        let e01 = PathBuf::from("/tv/Foo/Season 01/Foo S01E01.mkv");
        let double = PathBuf::from("/tv/Foo/Season 01/Foo S01E01-E02.mkv");
        let groups = group_editions(vec![e01.clone(), double.clone()]);
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(
            group_of(&groups, &e01).map(alt_paths),
            Some(vec![double.as_path()])
        );
        assert_eq!(group_of(&groups, &double), Some(&[][..]));
        assert!(
            folded_copies(&groups).is_empty(),
            "a double that is still an item keeps its row and its state"
        );
    }

    /// V844: with a single for both episodes the double is no item of its
    /// own: it is a version of both, and its row folds into each without its
    /// resume point, which is in the double's timeline.
    #[test]
    fn a_double_with_both_singles_folds_into_both() {
        let e01 = PathBuf::from("/tv/Foo/Season 01/Foo S01E01.mkv");
        let e02 = PathBuf::from("/tv/Foo/Season 01/Foo S01E02.mkv");
        let double = PathBuf::from("/tv/Foo/Season 01/Foo S01E01-E02 Bluray-2160p.mkv");
        let groups = group_editions(vec![double.clone(), e01.clone(), e02.clone()]);
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(
            group_of(&groups, &e01).map(alt_paths),
            Some(vec![double.as_path()])
        );
        assert_eq!(
            group_of(&groups, &e02).map(alt_paths),
            Some(vec![double.as_path()])
        );
        assert_eq!(
            folded_copies(&groups),
            vec![Fold {
                copy: double,
                into: vec![
                    (e01, pharos_core::FoldResume::Drop),
                    (e02, pharos_core::FoldResume::Drop),
                ],
            }]
        );
    }

    /// Two doubles of the same span are one item, the better one primary, and
    /// the other's resume point carries over.
    #[test]
    fn two_doubles_of_one_span_are_one_item() {
        let sd = PathBuf::from("/tv/Foo/Season 01/Foo S01E01-E02 HDTV-720p.mkv");
        let hd = PathBuf::from("/tv/Foo/Season 1/Foo S01E01-E02 WEBDL-1080p.mkv");
        let groups = group_editions(vec![sd.clone(), hd.clone()]);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(
            group_of(&groups, &hd).map(alt_paths),
            Some(vec![sd.as_path()])
        );
        assert_eq!(
            folded_copies(&groups)
                .first()
                .and_then(|f| f.into.first())
                .map(|(_, resume)| *resume),
            Some(pharos_core::FoldResume::Carry)
        );
    }

    /// A copy folding into items of different spans carries its resume
    /// point only into the one whose span it shares.
    #[test]
    fn a_copy_carries_its_resume_only_into_an_item_of_its_span() {
        let e01 = PathBuf::from("/tv/Foo/Season 01/Foo S01E01.mkv");
        let hd = PathBuf::from("/tv/Foo/Season 01/Foo S01E01-E02 WEBDL-1080p.mkv");
        let sd = PathBuf::from("/tv/Foo/Season 1/Foo S01E01-E02 HDTV-720p.mkv");
        let groups = group_editions(vec![e01.clone(), hd.clone(), sd.clone()]);
        assert_eq!(
            folded_copies(&groups),
            vec![Fold {
                copy: sd,
                into: vec![
                    (e01, pharos_core::FoldResume::Drop),
                    (hd, pharos_core::FoldResume::Carry),
                ],
            }]
        );
    }

    /// V844: `S01E01-02` is a double like `S01E01-E02`, so it never takes
    /// over E01 from a single; a release tag after a dash is no span.
    #[test]
    fn a_bare_dash_range_is_a_span() {
        assert_eq!(episode_span_end("Foo S01E01-02.mkv"), Some(2));
        assert_eq!(episode_span_end("Foo S01E01-E02.mkv"), Some(2));
        assert_eq!(episode_span_end("Foo S01E01E02-03.mkv"), Some(3));
        assert_eq!(episode_span_end("Foo S01E01-1080p.mkv"), None);
        assert_eq!(episode_span_end("Foo S01E01-720p.mkv"), None);
        let single = PathBuf::from("/tv/Foo/Season 01/Foo S01E01.mkv");
        let double = PathBuf::from("/tv/Foo/Season 01/Foo S01E01-02.mkv");
        let facts = HashMap::from([
            (
                single.clone(),
                CopyFacts {
                    height: Some(1080),
                    size: Some(1),
                },
            ),
            (
                double.clone(),
                CopyFacts {
                    height: Some(1080),
                    size: Some(2),
                },
            ),
        ]);
        let groups = group_copies(
            group_by_edition(vec![single.clone(), double.clone()]),
            &facts,
            &mut CopyTally::default(),
        );
        assert_eq!(
            group_of(&groups, &single).map(alt_paths),
            Some(vec![double.as_path()])
        );
        assert_eq!(group_of(&groups, &double), Some(&[][..]));
    }

    /// A season pack groups only with packs of exactly its span: it is no
    /// copy of each of its ten episodes.
    #[test]
    fn a_long_span_groups_only_with_its_exact_span() {
        let a = PathBuf::from("/tv/Foo/Season 01/Foo S01E01-E10.mkv");
        let b = PathBuf::from("/tv/Foo/Season 1/Foo S01E01-E10.mkv");
        let e03 = PathBuf::from("/tv/Foo/Season 01/Foo S01E03.mkv");
        let groups = group_editions(vec![a.clone(), b.clone(), e03.clone()]);
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(
            group_of(&groups, &a).map(alt_paths),
            Some(vec![b.as_path()])
        );
        assert_eq!(group_of(&groups, &e03), Some(&[][..]));
    }

    /// V846: copies of one film are one item, the best quality primary.
    #[test]
    fn copies_of_a_film_are_one_item() {
        let sd = PathBuf::from("/films/Heat (1995) [720p]/Heat.1995.720p.HDTV.mkv");
        let hd = PathBuf::from("/films/Heat (1995)/Heat (1995) Bluray-1080p.mkv");
        let three_d = PathBuf::from("/films/3D/Heat (1995) - 3D.HSBS.mkv");
        let groups = group_editions(vec![sd.clone(), hd.clone(), three_d.clone()]);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].0, hd);
        assert_eq!(
            alt_paths(&groups[0].1),
            vec![sd.as_path(), three_d.as_path()]
        );
        assert_eq!(
            folded_copies(&groups)
                .iter()
                .flat_map(|f| f.into.iter().map(|(_, r)| *r))
                .collect::<Vec<_>>(),
            [pharos_core::FoldResume::Carry; 2]
        );
    }

    /// V846: a film groups only with files whose own names state the same
    /// title and year; editions, other years, songs and trailers stay apart.
    #[test]
    fn a_film_groups_only_with_its_own_title_and_year() {
        let groups = group_editions(vec![
            PathBuf::from("/films/Film A (1971)/Film A (1971).mkv"),
            PathBuf::from("/films/Film A (1996)/Film A (1996).mkv"),
            PathBuf::from("/films/Heat (1995)/Heat (1995).mkv"),
            PathBuf::from("/films/Heat (1995)/trailer.mkv"),
            PathBuf::from("/films/Other/Heat (1995) - Director's Cut.mkv"),
            PathBuf::from("/films/Untitled/Heat.mkv"),
            PathBuf::from("/films/Untitled/Film A.mkv"),
            PathBuf::from("/music/Heat (1995)/Heat (1995).flac"),
        ]);
        assert_eq!(groups.len(), 6, "{groups:?}");
        let heat = groups
            .iter()
            .find(|(p, _)| p == Path::new("/films/Heat (1995)/Heat (1995).mkv"))
            .map(|(_, alts)| {
                alt_paths(alts)
                    .into_iter()
                    .map(Path::to_path_buf)
                    .collect::<Vec<_>>()
            });
        assert_eq!(
            heat,
            Some(vec![
                PathBuf::from("/films/Untitled/Heat.mkv"),
                PathBuf::from("/films/Other/Heat (1995) - Director's Cut.mkv"),
            ]),
            "a cut is a version of its film (V962); a yearless copy joins the one year its title is held in (V971)"
        );
        let others = groups
            .iter()
            .filter(|(p, _)| !p.ends_with("Heat (1995).mkv"));
        assert!(
            others.into_iter().all(|(_, alts)| alts.is_empty()),
            "{groups:?}"
        );
    }

    /// B1011 / V970 — doubles prod still showed after #1094: a scene tail
    /// after the year (`DTS-HD.MA.7.1-LTT`, `MKV.AC3.DTS.Eng.NL.Subs`, a group
    /// mid-tail `3Li BluRay 720p`), underscores and an inner `The`. A disc or
    /// part in such a tail still tells two files apart.
    #[test]
    fn the_film_doubles_left_after_the_first_fold_are_copies() {
        for (primary, copy) in [
            (
                "/m/Everest (2015)/Everest (2015) Bluray-1080p.mkv",
                "/m/Everest.2015.BluRay.1080p.x264.DTS-HD.MA.7.1-LTT/Everest.2015.BluRay.1080p.x264.DTS-HD.MA.7.1-LTT.mkv",
            ),
            (
                "/m/Johnny English Reborn (2011)/Johnny English Reborn (2011) WEBRip-1080p.mkv",
                "/m/Johnny.English.Reborn.2011.1080p.MKV.AC3.DTS.Eng.NL.Subs/Johnny.English.Reborn.2011.1080p.MKV.AC3.DTS.Eng.NL.Subs.mkv",
            ),
            (
                "/m/Lara Croft Tomb Raider (2001)/Lara Croft - Tomb Raider (2001) WEBDL-1080p.mkv",
                "/m/Lara Croft Tomb Raider 2001 BluRay 720p DTS x264-3Li/Lara Croft Tomb Raider 2001 3Li BluRay 720p.mkv",
            ),
            (
                "/m/Superbad (2007)/Superbad (2007) Bluray-1080p.mkv",
                "/m/Superbad 2007 Unrated Extended 1080p BluRay/Superbad_2007_Unrated_Extended_1080p_BluRay_BHD264_AAC51_MP4-FASM.mp4",
            ),
            (
                "/m/Hobbit The Battle of the Five Armies, The (2014)/The Hobbit - The Battle of the Five Armies (2014) WEBDL-1080p.mkv",
                "/m/The.Hobbit.Battle.Of.The.Five.Armies.2014.1080p.WEBRip.x264-tomcat12[ETRG]/The.Hobbit.Battle.Of.The.Five.Armies.2014.1080p.WEBRip.x264-tomcat12[ETRG].mp4",
            ),
        ] {
            let groups = group_editions(vec![PathBuf::from(primary), PathBuf::from(copy)]);
            assert_eq!(groups.len(), 1, "{copy} is a copy of {primary}: {groups:?}");
        }
        for (a, b) in [
            (
                "/m/Final Destination (2000)/Final Destination (2000) BluRay cd1.mkv",
                "/m/Final Destination (2000)/Final Destination (2000) BluRay cd2.mkv",
            ),
            (
                "/m/Heat (1995)/Heat (1995) Bluray-1080p.mkv",
                "/m/Heat (1995)/Heat (1995) Part 2 Bluray-1080p.mkv",
            ),
            // B1025 / V984
            (
                "/m/Heat (1995)/Heat (1995) Bluray-1080p.mkv",
                "/m/Heat (1995)/Heat.1995.Trailer.720p.mkv",
            ),
            (
                "/m/Heat (1995)/Heat (1995) Bluray-1080p.mkv",
                "/m/Heat (1995)/Heat.1995.Behind.The.Scenes.1080p.WEB.mkv",
            ),
        ] {
            let groups = group_editions(vec![PathBuf::from(a), PathBuf::from(b)]);
            assert_eq!(groups.len(), 2, "{b} stays apart from {a}: {groups:?}");
        }
    }

    /// B1012 / V971 — a copy named with no year, in a folder with none, joins
    /// its title when the library holds that title in one year only; a title
    /// held in two years (a remake) leaves it apart.
    #[test]
    fn a_yearless_copy_joins_the_one_year_its_title_is_held_in() {
        for (primary, copy) in [
            (
                "/m/Secret Life of Pets, The (2016)/The Secret Life of Pets (2016) WEBRip-1080p.mkv",
                "/m/The Secret Life of Pets/The Secret Life of Pets.avi",
            ),
            (
                "/m/Transformers Age of Extinction/Transformers - Age of Extinction (2014) WEBDL-1080p.mkv",
                "/m/Transformers Age of Extinction/Transformers Age of Extinction.mkv",
            ),
            (
                "/m/Taken 3 (2015)/Taken 3.mkv",
                "/m/Taken 3/Taken.3.1080p.BluRay.x265-n0m1.mkv",
            ),
        ] {
            let groups = group_editions(vec![PathBuf::from(primary), PathBuf::from(copy)]);
            assert_eq!(groups.len(), 1, "{copy} is a copy of {primary}: {groups:?}");
        }
        let groups = group_editions(vec![
            PathBuf::from("/m/The Lion King (1994)/The Lion King (1994).mkv"),
            PathBuf::from("/m/The Lion King (2019)/The Lion King (2019).mkv"),
            PathBuf::from("/m/Lion King/The Lion King.mkv"),
        ]);
        assert_eq!(
            groups.len(),
            3,
            "a remade title leaves a yearless copy apart: {groups:?}"
        );
    }

    /// B1014 / V973 — a letterboxed rip ranks by its width's 16:9 class, so a
    /// 1920x800 Blu-ray is no lower than a 1440x1080 one.
    #[test]
    fn a_letterboxed_copy_ranks_by_its_width() {
        assert_eq!(picture_lines(Some(1920), Some(800)), Some(1080));
        assert_eq!(picture_lines(Some(1440), Some(1080)), Some(1080));
        assert_eq!(picture_lines(Some(3840), Some(1600)), Some(2160));
        assert_eq!(picture_lines(None, Some(720)), Some(720));
        assert_eq!(picture_lines(Some(1280), None), Some(720));
        assert_eq!(picture_lines(None, None), None);
    }

    #[test]
    fn a_film_key_folds_case_and_punctuation() {
        assert_eq!(
            film_copy_key(Path::new("/f/The.Matrix.1999.1080p.BluRay.mkv")),
            film_copy_key(Path::new("/g/The Matrix (1999) WEBDL-720p.mp4")),
        );
        assert_eq!(film_copy_key(Path::new("/f/The Matrix.mkv")), None);
    }

    /// V846: a name with no year takes its folder's year when it names the
    /// folder's title, and a title's article may lead or trail.
    #[test]
    fn a_film_named_without_a_year_takes_its_folders() {
        let thing = film_copy_key(Path::new(
            "/m/The Thing (1982)/The Thing (1982) Bluray-1080p.mkv",
        ));
        assert_eq!(thing, Some(("thing".to_string(), 1982)));
        assert_eq!(
            film_copy_key(Path::new("/m/Thing, The (1982)/The Thing.mkv")),
            thing
        );
        assert_eq!(
            film_copy_key(Path::new(
                "/m/X-Men Apocalypse (2016)/X-Men - Apocalypse (2016) 3D HSBS.mkv"
            )),
            film_copy_key(Path::new(
                "/m/X-Men - Apocalypse (2016)/X-Men Apocalypse (2016).mkv"
            )),
        );
        assert_eq!(
            film_copy_key(Path::new(
                "/m/X-Men Apocalypse (2016) [3D] [HSBS] [YTS.AG]/X-Men.Apocalypse.2016.3D.HSBS.BluRay.x264-[YTS.AG].mp4"
            )),
            film_copy_key(Path::new(
                "/m/X-Men Apocalypse (2016)/X-Men - Apocalypse (2016) WEBDL-1080p.mkv"
            )),
            "a release group is how a copy was made"
        );
        assert_eq!(
            film_copy_key(Path::new("/m/Heat.1995.1080p.BluRay.x264-SPARKS.mkv")),
            Some(("heat".to_string(), 1995))
        );
        for extra in [
            "trailer.mkv",
            "The Thing - Behind the Scenes.mkv",
            "Thing.mkv.part",
        ] {
            let key = film_copy_key(&Path::new("/m/Thing, The (1982)").join(extra));
            assert_ne!(key, thing, "{extra}");
        }
        assert_eq!(film_copy_key(Path::new("/m/Untitled/The Thing.mkv")), None);
        for named in [
            "The Thing cd1.mp4",
            "The Thing Director’s Cut.mkv",
            "The Thing [Extended].mkv",
            "The Thing (Director's Cut).mkv",
        ] {
            let key = film_copy_key(&Path::new("/m/Thing, The (1982)").join(named));
            assert_eq!(key, thing, "{named}");
        }
        assert_eq!(
            film_copy_key(Path::new("/m/The Final Cut (2004)/The Final Cut.mkv")),
            Some(("final cut".to_string(), 2004)),
            "a title that ends in a cut word still names its folder"
        );
        assert_ne!(
            film_copy_key(Path::new("/m/The Thing (2011)/The Thing.mkv")),
            thing,
            "a remake's folder year keeps it apart"
        );
    }

    /// V960: a scene name's release group and codec tail never keep a copy
    /// apart from its film (B1001).
    #[test]
    fn a_scene_release_name_is_a_copy_of_its_film() {
        let key = |p: &str| film_copy_key(Path::new(p));
        let beowulf = key("/m/Beowulf (2007)/Beowulf (2007) Bluray-1080p.mkv");
        assert_eq!(beowulf, Some(("beowulf".to_string(), 2007)));
        assert_eq!(
            key("/m/Beowulf (2007) [1080p]/Beowulf.2007.1080p.BrRip.x264.Deceit.mp4"),
            beowulf
        );
        let fd3 = key("/m/Final Destination 3 (2006)/Final Destination 3 (2006) WEBDL-1080p.mkv");
        assert_eq!(fd3, Some(("final destination 3".to_string(), 2006)));
        for copy in [
            "/m/x/Final Destination 3 2006 BluRay 720p DTS x264-MgB [ETRG].mkv",
            "/m/x/Final.Destination.3.2006.1080p.BluRay.x264.AC3-ETRG.mp4",
            "/m/x/Final Destination 3 2006 1080p YIFY.mp4",
        ] {
            assert_eq!(key(copy), fd3, "{copy}");
        }
        assert_eq!(
            key("/m/x/Captain.America.The.Winter.Soldier.2014.720p.BluRay.x264.DTS-WiKi.mkv"),
            key("/m/y/Captain America - The Winter Soldier (2014) Bluray-1080p.mkv"),
        );
    }

    /// V961: a release group's promo clip is never indexed.
    #[test]
    fn a_release_promo_is_not_walked() {
        assert!(is_release_promo(Path::new(
            "/m/Final Destination 3 (2006)/ETRG.mp4"
        )));
        assert!(is_release_promo(Path::new("/m/X/RARBG.mkv")));
        assert!(!is_release_promo(Path::new("/m/X/RARBG.txt")));
        assert!(!is_release_promo(Path::new(
            "/m/Yify (2020)/Yify Story (2020).mkv"
        )));
    }

    /// V961: a release's sample clip is never indexed (B1002).
    #[test]
    fn a_release_sample_is_not_walked() {
        for sample in [
            "Black Panther (2018)/Sample/black.panther.2018.1080p.bluray.x264-sparks.sample.mkv",
            "X/Final Destination 3 2006 BluRay 720p DTS x264-MgB (Sample).mkv",
            "X/Final.Destination.5.2011.1080p.BluRay.x264.AC3-ETRG.Sample.mp4",
            "The Golem (2018)/Sample.mkv",
            "Heat (1995)/Heat (1995)-sample.mkv",
            // B1013 / V972
            "X/sample-The Last Starfighter (1984) 25th Anniversary Ed.mkv",
        ] {
            assert!(is_release_sample(Path::new(sample)), "{sample}");
        }
        for kept in [
            "Heat (1995)/Heat (1995) Bluray-1080p.mkv",
            "Samples (2020)/Samples (2020).mkv",
            "Music/Artist/Album/03 - Sample.flac",
            "Sample Size (2019)/Sample Size (2019).mkv",
        ] {
            assert!(!is_release_sample(Path::new(kept)), "{kept}");
        }
    }

    /// V846: a film's extras, cuts, parts and a show named by episode number
    /// are never copies of the film; a 3D rip of a cut is a copy of that cut.
    #[test]
    fn a_film_name_that_says_more_than_its_release_is_its_own_title() {
        let film = film_copy_key(Path::new("/f/Heat (1995)/Heat (1995) Bluray-1080p.mkv"));
        assert_eq!(film, Some(("heat".to_string(), 1995)));
        for other in [
            "Heat (1995)-trailer.mkv",
            "Heat (1995)-sample.mkv",
            "Heat.1995.sample.mkv",
            "Heat (1995) - Behind the Scenes.mkv",
            "Heat (1995) Extras.mkv",
            "Heat (1995) - part1.mkv",
            "Heat (1995) - part2.mkv",
            "Heat (1995) cd2.avi",
        ] {
            let key = film_copy_key(&Path::new("/f/Heat (1995)").join(other));
            assert_ne!(key, film, "{other}");
        }
        assert_ne!(
            film_copy_key(Path::new("/f/Heat (1995) - part1.mkv")),
            film_copy_key(Path::new("/f/Heat (1995) - part2.mkv")),
        );
        // V962, V963: a cut, an edition or a lone cd1 is a version of the film.
        for cut in [
            "Heat (1995) Director's Cut.mkv",
            "Heat (1995) - Director's Cut - 3D.HSBS.mkv",
            "Heat.1995.Directors.Cut.1080p.BluRay.x264-SPARKS.mkv",
            "Heat (1995) {edition-Director's Cut}.mkv",
            "Heat (1995) Remastered.mkv",
            "Heat (1995) IMAX.mkv",
            "Heat (1995) Unrated Extended Cut.mkv",
            "Heat (1995) [Extended].mkv",
            "Heat (1995) cd1.avi",
        ] {
            let key = film_copy_key(&Path::new("/f/Heat (1995)").join(cut));
            assert_eq!(key, film, "{cut}");
        }
        assert_ne!(
            film_copy_key(Path::new(
                "/f/The Final Cut (2004)/The Final Cut (2004).mkv"
            )),
            None,
            "a cut word before the year is the title",
        );
        assert_eq!(
            film_copy_key(Path::new(
                "/f/Heat.1995.2160p.UHD.BluRay.x265.HDR.DDP5.1.mkv"
            )),
            film,
        );
        assert_ne!(
            film_copy_key(Path::new(
                "/tv/Planet Earth (2006)/Planet Earth (2006) - 01 - From Pole to Pole.mkv"
            )),
            film_copy_key(Path::new(
                "/tv/Planet Earth (2006)/Planet Earth (2006) - 02 - Mountains.mkv"
            )),
        );
    }

    /// V847: a bracket that says more than a format keeps songs, discs and
    /// remixes apart, and a disc folder does not drop the artist.
    #[test]
    fn a_song_tag_that_names_a_version_or_disc_keeps_it_apart() {
        let key = |p: &str| song_copy_key(Path::new(p));
        let song = key("/m/A/Single/Song.flac");
        assert_ne!(key("/m/A/Single/Song [Instrumental].flac"), song);
        assert_ne!(key("/m/A/Single/Song [Live].flac"), song);
        assert_ne!(key("/m/A/Single/Song (Remix v2).flac"), song);
        assert_ne!(key("/m/A/Single/Song (Web Version).flac"), song);
        assert_ne!(
            key("/m/A/Album/Interlude (1).flac"),
            key("/m/A/Album/Interlude (2).flac"),
        );
        assert_eq!(
            key("/m/A/Album/Song [FLAC 44.1].flac"),
            key("/m/A/Album/Song.mp3"),
        );
        assert_eq!(key("/m/A/Single/Song [FLAC 24-96].flac"), song);
        assert_eq!(key("/m/A/Single (2001) [WEB]/Song.mp3"), song);
        assert_ne!(
            key("/m/A/Album [CD1]/01 Intro.flac"),
            key("/m/A/Album [CD2]/01 Intro.flac"),
        );
        assert_ne!(
            key("/m/A/Album {Deluxe}/01 Intro.flac"),
            key("/m/A/Album/01 Intro.flac"),
        );
        assert_ne!(
            key("/m/A/Greatest Hits/CD1/01 Intro.flac"),
            key("/m/B/Greatest Hits/CD1/01 Intro.flac"),
        );
    }

    /// A song's name is never read for a 3D packing, so `01 Tab.mp3` ranks by
    /// size like any other song.
    #[test]
    fn a_song_is_never_ranked_as_3d() {
        let small = PathBuf::from("/m/A/Album/01 Tab.mp3");
        let big = PathBuf::from("/m/A/Album/01 Tab.flac");
        let rank = CopyRank::of(Some(&big), None, &HashMap::new());
        assert!(!rank.is_3d, "{rank:?}");
        let rank = CopyRank::of(Some(&small), None, &HashMap::new());
        assert!(!rank.is_3d, "{rank:?}");
    }

    /// V847: copies of one song, beside each other or in a second rip of the
    /// album, are one item, the bigger (higher bitrate) file its primary.
    #[test]
    fn copies_of_a_song_are_one_item() {
        let flac = PathBuf::from("/music/Artist/Album (2001) [FLAC]/01 - Song.flac");
        let mp3 = PathBuf::from("/music/Artist/Album/01 - Song.mp3");
        let ogg = PathBuf::from("/music/Artist/Album/01 - Song.ogg");
        let size = |n| CopyFacts {
            height: None,
            size: Some(n),
        };
        let facts = HashMap::from([
            (flac.clone(), size(30_000_000)),
            (mp3.clone(), size(9_000_000)),
            (ogg.clone(), size(5_000_000)),
        ]);
        let mut tally = CopyTally::default();
        let groups = group_copies(
            group_by_edition(vec![mp3.clone(), ogg.clone(), flac.clone()]),
            &facts,
            &mut tally,
        );
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].0, flac);
        assert_eq!(alt_paths(&groups[0].1), vec![mp3.as_path(), ogg.as_path()]);
        assert_eq!(tally.0.get(&CopyDecision::Size), Some(&1));
        assert_eq!(
            folded_copies(&groups)
                .iter()
                .flat_map(|f| f.into.iter().map(|(_, r)| *r))
                .collect::<Vec<_>>(),
            [pharos_core::FoldResume::Carry; 2]
        );
    }

    /// V847: a song groups only with files of its own name under the same two
    /// folders, release tags aside; other songs, artists, editions and discs
    /// stay apart.
    #[test]
    fn a_song_groups_only_with_its_own_name_and_folders() {
        let groups = group_editions(vec![
            PathBuf::from("/music/A/Album/01 Intro.flac"),
            PathBuf::from("/music/B/Album/01 Intro.flac"),
            PathBuf::from("/music/A/Album/02 Song.flac"),
            PathBuf::from("/music/A/Album (Deluxe)/01 Intro.flac"),
            PathBuf::from("/music/A/Album (CD 2)/01 Intro.flac"),
            PathBuf::from("/music/A/Album/01 Intro (Live).flac"),
            PathBuf::from("/music/A/Album/01 Intro.mkv"),
        ]);
        assert_eq!(groups.len(), 7, "{groups:?}");
        assert_eq!(
            song_copy_key(Path::new("/m/A/Album [MP3 320]/01 Intro.mp3")),
            song_copy_key(Path::new("/m/a/album (320 kbps)/01_intro [web].flac")),
        );
        assert_ne!(song_copy_key(Path::new("/m/A/Album/01 Intro.flac")), None);
    }

    /// Two shows of one name in one folder, told apart by the year their
    /// names state, are not copies of each other; copies of one still are.
    #[test]
    fn same_named_shows_of_different_years_are_not_copies() {
        let groups = group_editions(vec![
            PathBuf::from("/tv/Show A/Season 1/Show A (1961) - S01E01 - The Old Shore.mkv"),
            PathBuf::from("/tv/Show A/Season 1/Show A (1996) - S01E01 - The New Start.mkv"),
            PathBuf::from("/tv/Show A/Season 01/Show A (1996) - S01E01 - The New Start.avi"),
            PathBuf::from("/tv/Foo/Season 1/Foo - S01E02 - 1969 HDTV-720p.mkv"),
            PathBuf::from("/tv/Foo/Season 1/Foo - S01E02 - 1969 WEBDL-1080p.mkv"),
        ]);
        assert_eq!(groups.len(), 3, "{groups:?}");
        assert_eq!(name_show_year("Foo - S01E02 - 1969.mkv"), None);
        assert_eq!(name_show_year("Show.C.1988.S01E01.mkv"), Some(1988));
    }

    /// A theme song belongs to its title folder: two shows of one name are
    /// not copies of each other (`Show B`, `Show B (1996)`).
    #[test]
    fn a_theme_song_is_never_a_copy() {
        let groups = group_editions(vec![
            PathBuf::from("/tv/Show B/theme.mp3"),
            PathBuf::from("/tv/Show B (1996)/theme.mp3"),
            PathBuf::from("/tv/Show A/theme-music/Main Title.mp3"),
            PathBuf::from("/tv/Show A (1996)/theme-music/Main Title.mp3"),
        ]);
        assert_eq!(groups.len(), 4, "{groups:?}");
        assert!(groups.iter().all(|(_, alts)| alts.is_empty()));
    }

    /// A song has no picture, so its copies never send the scan to the store
    /// for a stored height.
    #[test]
    fn copies_of_a_song_are_never_looked_up() {
        let groups = group_by_edition(vec![
            PathBuf::from("/music/A/Album/01 Intro.flac"),
            PathBuf::from("/music/A/Album/01 Intro.mp3"),
        ]);
        let candidates = copy_candidates(&groups);
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|(_, look_up)| !look_up));
    }

    /// V845: the highest-quality copy is the primary: the taller picture
    /// first, as a stored probe or else the file name states it, then the
    /// bigger file.
    #[test]
    fn the_best_quality_copy_is_the_primary() {
        let sd = PathBuf::from("/tv/Foo/Season 01/Foo S01E03 - Pilot HDTV-720p.mkv");
        let hd = PathBuf::from("/tv/Foo/Season 1/Foo S01E03 - Pilot WEBDL-1080p.mkv");
        let groups = group_editions(vec![sd.clone(), hd.clone()]);
        assert_eq!(groups[0].0, hd, "the name's 1080p beats its 720p");

        // A stored probe beats the name: this "720p" file is really 2160 tall.
        let facts = HashMap::from([
            (
                sd.clone(),
                CopyFacts {
                    height: Some(2160),
                    size: Some(1),
                },
            ),
            (
                hd.clone(),
                CopyFacts {
                    height: None,
                    size: Some(2),
                },
            ),
        ]);
        let groups = group_copies(
            group_by_edition(vec![sd.clone(), hd.clone()]),
            &facts,
            &mut CopyTally::default(),
        );
        assert_eq!(groups[0].0, sd);

        // Same height: the bigger file, which at one length is the higher bitrate.
        let small = PathBuf::from("/tv/Foo/Season 01/Foo S01E03 - Pilot.mkv");
        let big = PathBuf::from("/tv/Foo/Season 1/Foo S01E03 - Pilot.mkv");
        let facts = HashMap::from([
            (
                small.clone(),
                CopyFacts {
                    height: Some(1080),
                    size: Some(900),
                },
            ),
            (
                big.clone(),
                CopyFacts {
                    height: Some(1080),
                    size: Some(4_000),
                },
            ),
        ]);
        let groups = group_copies(
            group_by_edition(vec![small.clone(), big.clone()]),
            &facts,
            &mut CopyTally::default(),
        );
        assert_eq!(groups[0].0, big);
        assert_eq!(alt_paths(&groups[0].1), vec![small.as_path()]);
    }

    /// A single episode stays primary over a better double: the double starts
    /// at the episode before, and is offered as a version.
    #[test]
    fn a_single_episode_outranks_a_better_double() {
        let single = PathBuf::from("/tv/Foo/Season 01/Foo S01E02 HDTV-480p.avi");
        let double = PathBuf::from("/tv/Foo/Season 01/Foo S01E01-E02 Bluray-2160p.mkv");
        let groups = group_editions(vec![double.clone(), single.clone()]);
        assert_eq!(
            group_of(&groups, &single).map(alt_paths),
            Some(vec![double.as_path()])
        );
    }

    #[test]
    fn a_release_name_states_its_height() {
        for (stem, height) in [
            ("Show - S01E01 - Pilot WEBDL-1080p", Some(1080)),
            ("Film (2017) [2160p] WEB-DL", Some(2160)),
            ("Film.2017.4K.HDR", Some(2160)),
            ("Show S01E01 HDTV-720p Proper", Some(720)),
            ("Show S01E01 1080i", Some(1080)),
            ("Show S01E01 WEB-DL 1080p60", Some(1080)),
            ("Film 2160p50 HDR", Some(2160)),
            ("Show S01E01", None),
            ("Room 1080", None),
            ("x1080p", None),
        ] {
            assert_eq!(name_height(stem), height, "{stem}");
        }
    }

    /// The `decided_by` labels are a dashboard contract: stable and distinct.
    #[test]
    fn copy_decision_labels_are_distinct() {
        let labels: HashSet<&str> = CopyDecision::ALL.iter().map(|d| d.label()).collect();
        assert_eq!(labels.len(), CopyDecision::ALL.len());
    }

    /// A scan pass reports, for every rule, how many episodes with copies it
    /// decided, so a dashboard can tell a quality pick from a tie broken by
    /// path. A lone episode is not counted.
    #[test]
    fn a_scan_pass_reports_the_rule_that_chose_each_primary() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut tally = CopyTally::default();
        group_copies(
            group_by_edition(vec![
                PathBuf::from("/tv/Foo/Season 01/Foo S01E03.mkv"),
                PathBuf::from("/tv/Foo/Season 1/Foo S01E03.mkv"),
                PathBuf::from("/tv/Foo/Season 01/Foo S01E04.mkv"),
                PathBuf::from("/tv/Foo/Season 1/Foo S01E04.avi"),
                PathBuf::from("/tv/Foo/Season 01/Foo S01E05.mkv"),
            ]),
            &HashMap::new(),
            &mut tally,
        );
        tally.publish();
        let mut seen: Vec<(String, u64)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter_map(|(k, _, _, value)| {
                let key = k.key();
                let DebugValue::Gauge(n) = value else {
                    return None;
                };
                (key.name() == "pharos_scan_copy_primaries").then(|| {
                    let label = key
                        .labels()
                        .find(|l| l.key() == "decided_by")
                        .map(|l| l.value().to_string())
                        .unwrap_or_default();
                    (label, n.into_inner() as u64)
                })
            })
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [
                ("2d".to_string(), 0),
                ("container".to_string(), 1),
                ("height".to_string(), 0),
                ("path".to_string(), 1),
                ("refused_other_series".to_string(), 0),
                ("single_episode".to_string(), 0),
                ("size".to_string(), 0),
            ]
        );
    }

    /// V721: a 3D copy never becomes the primary over a 2D copy, even when
    /// the 3D copy is the `.mkv` and the 2D copy the `.mp4`.
    #[test]
    fn a_3d_copy_never_becomes_the_primary_over_a_2d_copy() {
        let three_d = PathBuf::from("/tv/Show/Season 1/Show - S01E02 - Pilot - 3D.HSBS.mkv");
        let two_d = PathBuf::from("/tv/Show/Season 01/Show - S01E02 - Pilot.mp4");
        let groups = group_editions(vec![three_d.clone(), two_d.clone()]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, two_d);
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[0].1[0].1, three_d);
    }

    /// The fold guard a scan pass builds for `paths` (V894).
    fn guard_for(
        paths: &[PathBuf],
        overrides: Vec<pharos_core::SeriesOverride>,
        ambiguous: &[&str],
    ) -> FoldGuard {
        let folders = crate::series_split::folders_of(paths);
        let ctx = crate::series_split::PlanContext {
            overrides,
            ..Default::default()
        };
        let plan = crate::series_split::plan(&folders, &ctx, 0);
        let ambiguous = ambiguous.iter().map(|k| (*k).to_string()).collect();
        FoldGuard::new(
            &folders,
            &plan,
            &ctx,
            ambiguous,
            crate::series_split::RepairMode::Apply,
        )
    }

    /// `paths` grouped behind `guard`, and the copies it kept apart.
    fn guarded_groups(paths: &[PathBuf], guard: &FoldGuard) -> (Vec<Group>, u64) {
        let mut tally = CopyTally::default();
        let out = group_copies_guarded(
            group_by_edition(paths.to_vec()),
            &HashMap::new(),
            guard,
            &mut tally,
        );
        let refused = tally
            .0
            .get(&CopyDecision::RefusedOtherSeries)
            .copied()
            .unwrap_or(0);
        (out, refused)
    }

    /// Show B folder holding both shows, and two unyeared copies of one
    /// episode number.
    fn show_b_with_copies(titles: [&str; 2]) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = [
            "/tv/Show B/Season 1/Show B (1971) - S01E01.mkv",
            "/tv/Show B/Season 3/Show B (1996) - S03E01.mkv",
            "/tv/Show B/Season 3/Show B (1996) - S03E02.mkv",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        paths.push(PathBuf::from(format!(
            "/tv/Show B/Season 1/Show B - S01E02{}.mkv",
            titles[0]
        )));
        paths.push(PathBuf::from(format!(
            "/tv/Show B/Season 01/Show B - S01E02{}.avi",
            titles[1]
        )));
        paths
    }

    /// V894, V975: a flagged show's guard covers the copies a show folder
    /// makes episodes, keyed by that show folder, not the sub-folder they sit in.
    #[test]
    fn a_flagged_shows_guard_covers_its_show_folder_copies() {
        let td = TempDir::new().unwrap();
        let show = td.path().join("Sword Art Online II");
        std::fs::create_dir_all(show.join("SAO II")).unwrap();
        std::fs::write(show.join("tvshow.nfo"), b"<tvshow/>").unwrap();
        let paths = vec![
            show.join("SAO II/SAO II - 14 L@mBerT.mp4"),
            show.join("SAO II/SAO II - 14 [1080p].mkv"),
        ];
        let key = show.to_string_lossy().into_owned();
        let guard = guard_for(&paths, vec![], &[key.as_str()]);
        assert_eq!(guard.refusal(&paths[0], &paths[1]), Some("numbers_only"));
    }

    /// V894: in a split folder, a matching season and episode number alone
    /// never folds two copies; agreeing episode titles do.
    #[test]
    fn in_a_split_folder_numbers_alone_never_fold_copies() {
        let paths = show_b_with_copies(["", ""]);
        let (out, refused) = guarded_groups(&paths, &guard_for(&paths, vec![], &[]));
        assert_eq!(out.len(), 5, "{out:?}");
        assert_eq!(refused, 1);
        assert_eq!(
            guard_for(&paths, vec![], &[]).refusal(&paths[3], &paths[4]),
            Some("numbers_only")
        );
        // The same files with no split to guard them fold as before.
        let (out, refused) = guarded_groups(&paths, &FoldGuard::default());
        assert_eq!((out.len(), refused), (4, 0));

        let titled = show_b_with_copies([" - The Third Way", " - The Third Way"]);
        let (out, refused) = guarded_groups(&titled, &guard_for(&titled, vec![], &[]));
        assert_eq!((out.len(), refused), (4, 0), "{out:?}");
    }

    /// V894: a show whose match is flagged folds its copies only on agreeing
    /// episode titles.
    #[test]
    fn a_flagged_shows_copies_fold_only_on_agreeing_titles() {
        let bare = [
            PathBuf::from("/tv/Show A/Season 1/Show A - S01E01.mkv"),
            PathBuf::from("/tv/Show A/Season 01/Show A - S01E01.avi"),
        ];
        let folder = parse_series_info(&bare[0]).unwrap().series_folder.unwrap();
        let (out, refused) = guarded_groups(&bare, &guard_for(&bare, vec![], &[&folder]));
        assert_eq!((out.len(), refused), (2, 1));
        let (out, refused) = guarded_groups(&bare, &guard_for(&bare, vec![], &[]));
        assert_eq!((out.len(), refused), (1, 0));

        let titled = [
            PathBuf::from("/tv/Show A/Season 1/Show A - S01E01 - The Old Second.mkv"),
            PathBuf::from("/tv/Show A/Season 01/Show A - S01E01 - The Old Second.avi"),
        ];
        let (out, refused) = guarded_groups(&titled, &guard_for(&titled, vec![], &[&folder]));
        assert_eq!((out.len(), refused), (1, 0));
    }

    /// V894: two copies whose file names give different episode titles are
    /// different episodes, in any show; an edition tag is no title.
    #[test]
    fn copies_titled_as_different_episodes_never_fold() {
        let paths = [
            PathBuf::from("/tv/Show/Season 1/Show - S01E02 - Pilot.mkv"),
            PathBuf::from("/tv/Show/Season 01/Show - S01E02 - The Long Night.mp4"),
        ];
        let apply = FoldGuard {
            mode: Some(crate::series_split::RepairMode::Apply),
            ..FoldGuard::default()
        };
        let (out, refused) = guarded_groups(&paths, &apply);
        assert_eq!((out.len(), refused), (2, 1));
        assert_eq!(apply.refusal(&paths[0], &paths[1]), Some("titles_differ"));
        // A dry run counts the copy it would keep apart and folds it.
        let dry = FoldGuard {
            mode: Some(crate::series_split::RepairMode::DryRun),
            ..FoldGuard::default()
        };
        let (out, refused) = guarded_groups(&paths, &dry);
        assert_eq!((out.len(), refused), (1, 1));
        // No guard, no check: the stand-alone listing folds as before.
        let (out, refused) = guarded_groups(&paths, &FoldGuard::default());
        assert_eq!((out.len(), refused), (1, 0));
        let tagged = [
            PathBuf::from("/tv/Show/Season 1/Show - S01E02 - Pilot - 3D.HSBS.mkv"),
            PathBuf::from("/tv/Show/Season 01/Show - S01E02 - Pilot.mp4"),
        ];
        assert_eq!(FoldGuard::default().refusal(&tagged[0], &tagged[1]), None);
    }

    /// V894/V896: a file an admin placed never folds with another copy,
    /// whatever `series_identity_repair` says.
    #[test]
    fn a_copy_an_admin_placed_never_folds() {
        let paths = [
            PathBuf::from("/tv/Show/Season 1/Show - S01E02 - Pilot.mkv"),
            PathBuf::from("/tv/Show/Season 01/Show - S01E02 - Pilot.mp4"),
        ];
        let folder = parse_series_info(&paths[0]).unwrap().series_folder.unwrap();
        let admin = pharos_core::SeriesOverride {
            id: 1,
            series_folder: folder.clone(),
            path: Some(paths[1].to_string_lossy().into_owned()),
            target_key: folder,
            target_name: None,
            target_year: None,
            source: pharos_core::OverrideSource::Admin,
            created_by: Some("admin".into()),
            created_at: 0,
        };
        let mut guard = guard_for(&paths, vec![admin], &[]);
        let (out, refused) = guarded_groups(&paths, &guard);
        assert_eq!((out.len(), refused), (2, 1));
        assert_eq!(guard.refusal(&paths[0], &paths[1]), Some("admin_placed"));
        // An admin's placement is kept apart in a dry run too.
        guard.mode = Some(crate::series_split::RepairMode::DryRun);
        let (out, refused) = guarded_groups(&paths, &guard);
        assert_eq!((out.len(), refused), (2, 1));
    }

    /// V894: two known stated show years that differ never fold, whatever
    /// key the two files share.
    #[test]
    fn copies_stating_different_show_years_never_fold() {
        let a = PathBuf::from("/tv/Show/Season 1/Show - S01E02 - Pilot.mkv");
        let b = PathBuf::from("/tv/Show/Season 01/Show - S01E02 - Pilot.mp4");
        let guard = FoldGuard {
            mode: Some(crate::series_split::RepairMode::Apply),
            stated: HashMap::from([(a.clone(), 1990), (b.clone(), 2014)]),
            ..FoldGuard::default()
        };
        assert_eq!(guard.refusal(&a, &b), Some("years_differ"));
        let (out, refused) = guarded_groups(&[a, b], &guard);
        assert_eq!((out.len(), refused), (2, 1));
    }

    #[tokio::test]
    async fn a_copy_with_its_own_row_folds_into_the_primary() {
        let td = TempDir::new().unwrap();
        let older = "Show/Season 1/Show - S01E02 - Pilot.mkv";
        let newer = "Show/Season 01/Show - S01E02 - Pilot.mkv";
        write_file(td.path(), older, b"first copy").await;
        let store = MemStore::default();
        let scanner = FsScanner::new(FakeProber::default());
        scanner.scan_into(td.path(), &store).await.unwrap();
        let before = store.list().await.unwrap();
        assert_eq!(before.len(), 1);
        let old_id = before[0].id;

        write_file(td.path(), newer, b"second copy").await;
        scanner.scan_into(td.path(), &store).await.unwrap();

        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1, "the copy's own row is swept");
        let item = &items[0];
        assert!(item.path.ends_with(newer));
        assert_eq!(item.probe.alternate_sources.len(), 1);
        assert!(item.probe.alternate_sources[0].path.ends_with(older));
        assert_eq!(
            *store.merged.lock().unwrap(),
            vec![(old_id, item.id, pharos_core::FoldResume::Carry)]
        );
    }

    /// V844: a double that is still the item for E02 keeps its row and is
    /// never folded into E01, so E01's state is its own.
    #[tokio::test]
    async fn a_double_that_is_still_an_item_keeps_its_row() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Show/Season 01/Show S01E01-E02.mkv", b"double").await;
        let store = MemStore::default();
        let scanner = FsScanner::new(FakeProber::default());
        scanner.scan_into(td.path(), &store).await.unwrap();
        write_file(td.path(), "Show/Season 01/Show S01E01.mkv", b"single").await;
        scanner.scan_into(td.path(), &store).await.unwrap();
        scanner.scan_into(td.path(), &store).await.unwrap();

        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 2, "{items:?}");
        let single = items
            .iter()
            .find(|i| i.path.ends_with("Show S01E01.mkv"))
            .unwrap();
        assert_eq!(single.probe.alternate_sources.len(), 1);
        assert!(single.probe.alternate_sources[0]
            .path
            .ends_with("Show S01E01-E02.mkv"));
        assert!(store.merged.lock().unwrap().is_empty());
    }

    /// V844: once both episodes have a single, the double's row folds into
    /// each of them without its resume point, and is swept.
    #[tokio::test]
    async fn a_double_with_both_singles_hands_its_state_to_both() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Show/Season 01/Show S01E01-E02.mkv", b"double").await;
        let store = MemStore::default();
        let scanner = FsScanner::new(FakeProber::default());
        scanner.scan_into(td.path(), &store).await.unwrap();
        let double = store.list().await.unwrap()[0].id;
        write_file(td.path(), "Show/Season 01/Show S01E01.mkv", b"single one").await;
        write_file(td.path(), "Show/Season 01/Show S01E02.mkv", b"single two").await;
        scanner.scan_into(td.path(), &store).await.unwrap();

        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 2, "the double's own row is swept: {items:?}");
        let merged = store.merged.lock().unwrap().clone();
        assert_eq!(merged.len(), 2, "{merged:?}");
        assert!(merged.iter().all(|(from, into, resume)| *from == double
            && items.iter().any(|i| i.id == *into)
            && *resume == pharos_core::FoldResume::Drop));
    }

    /// The store is asked for heights only where a copy's name states none.
    #[test]
    fn heights_are_looked_up_only_where_names_cannot_rank() {
        let groups = group_by_edition(vec![
            PathBuf::from("/tv/Foo/Season 01/Foo S01E01 HDTV-720p.mkv"),
            PathBuf::from("/tv/Foo/Season 1/Foo S01E01 WEBDL-1080p.mkv"),
            PathBuf::from("/tv/Foo/Season 01/Foo S01E02.mkv"),
            PathBuf::from("/tv/Foo/Season 1/Foo S01E02 WEBDL-1080p.mkv"),
            PathBuf::from("/tv/Foo/Season 01/Foo S01E03.mkv"),
        ]);
        let flags: Vec<(String, bool)> = copy_candidates(&groups)
            .into_iter()
            .map(|(p, f)| (p.display().to_string(), f))
            .collect();
        assert_eq!(
            flags,
            [
                (
                    "/tv/Foo/Season 01/Foo S01E01 HDTV-720p.mkv".to_string(),
                    false
                ),
                ("/tv/Foo/Season 01/Foo S01E02.mkv".to_string(), true),
                (
                    "/tv/Foo/Season 1/Foo S01E01 WEBDL-1080p.mkv".to_string(),
                    false
                ),
                (
                    "/tv/Foo/Season 1/Foo S01E02 WEBDL-1080p.mkv".to_string(),
                    true
                ),
            ]
        );
    }

    /// V845: a copy's height comes from its stored probe, on its own row or
    /// on the row it was folded into.
    #[tokio::test]
    async fn copy_facts_read_stored_heights() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.mkv", b"12345").await;
        let (a, b, c) = (
            td.path().join("a.mkv"),
            td.path().join("b.mkv"),
            td.path().join("c.mkv"),
        );
        let store = MemStore::default();
        let mut item = MediaItem {
            id: 1,
            path: a.clone(),
            ..Default::default()
        };
        item.probe.height = Some(720);
        item.probe.alternate_sources.push(AlternateMediaSource {
            id: "b".into(),
            path: b.clone(),
            container: None,
            video_codec: None,
            audio_codec: None,
            bitrate_bps: None,
            size_bytes: None,
            duration_ms: None,
            name: None,
            video_3d_format: None,
            probe: Some(Box::new(pharos_core::MediaProbe {
                height: Some(2160),
                ..Default::default()
            })),
            part: None,
        });
        store.put(item).await.unwrap();
        let facts = copy_facts(
            &store,
            &[(a.clone(), true), (b.clone(), true), (c.clone(), true)],
        )
        .await;
        assert_eq!(
            facts.get(&a),
            Some(&CopyFacts {
                height: Some(720),
                size: Some(5)
            })
        );
        assert_eq!(
            facts.get(&b),
            Some(&CopyFacts {
                height: Some(2160),
                size: None
            })
        );
        assert_eq!(facts.get(&c), Some(&CopyFacts::default()));
    }

    #[tokio::test]
    async fn a_copy_keeps_its_row_when_its_primary_cannot_be_probed() {
        let td = TempDir::new().unwrap();
        let good = "Show/Season 1/Show - S01E02 - Pilot.avi";
        let bad = "Show/Season 01/Show - S01E02 - Pilot.mkv";
        write_file(td.path(), good, b"playable copy").await;
        let store = MemStore::default();
        FsScanner::new(FakeProber::default())
            .scan_into(td.path(), &store)
            .await
            .unwrap();
        let kept = store.list().await.unwrap()[0].id;

        write_file(td.path(), bad, b"corrupt copy").await;
        let prober = FakeProber {
            force_fail_for: Some("Pilot.mkv".into()),
            ..Default::default()
        };
        FsScanner::new(prober)
            .scan_into(td.path(), &store)
            .await
            .unwrap();

        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, kept);
        assert!(items[0].path.ends_with(good));
        assert!(store.merged.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_watched_3d_file_is_flagged_3d() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Gravity (2013) - 3D.HTAB.mkv").await;
        let store = MemStore::default();
        let scanner = FsScanner::new(FakeProber::default());
        let scan_id = store.begin_scan(td.path()).await.unwrap();
        scanner
            .update_path(
                &td.path().join("Gravity (2013) - 3D.HTAB.mkv"),
                &store,
                scan_id,
            )
            .await
            .unwrap();
        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].probe.video_3d_format,
            Some(Video3DFormat::HalfTopAndBottom)
        );
    }

    #[tokio::test]
    async fn a_lone_3d_rip_is_its_own_item_flagged_3d() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "Gravity (2013) - 3D.HTAB.mkv").await;
        let items = FsScanner::new(FakeProber::default())
            .scan(td.path())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].probe.video_3d_format,
            Some(Video3DFormat::HalfTopAndBottom)
        );
        assert!(items[0].probe.alternate_sources.is_empty());
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

    #[test]
    fn resolve_3d_format_filename_fallback_movie_and_episode() {
        let mut movie_probe = pharos_core::MediaProbe::default();
        let movie_path = Path::new("/movies/Avatar.2009.1080p.3D.HSBS.mkv");
        resolve_item_3d_format(movie_path, MediaKind::Movie, &mut movie_probe);
        assert_eq!(
            movie_probe.video_3d_format,
            Some(Video3DFormat::HalfSideBySide)
        );

        let mut ep_probe = pharos_core::MediaProbe::default();
        let ep_path = Path::new("/shows/Show.S01E01.1080p.3D.HTAB.mkv");
        resolve_item_3d_format(ep_path, MediaKind::Episode, &mut ep_probe);
        assert_eq!(
            ep_probe.video_3d_format,
            Some(Video3DFormat::HalfTopAndBottom)
        );

        let mut audio_probe = pharos_core::MediaProbe::default();
        let audio_path = Path::new("/music/Track.3D.HSBS.flac");
        resolve_item_3d_format(audio_path, MediaKind::Audio, &mut audio_probe);
        assert_eq!(audio_probe.video_3d_format, None);
    }

    #[test]
    fn resolve_3d_format_container_metadata_precedence_over_conflicting_filename() {
        let mut probe = pharos_core::MediaProbe {
            video_3d_format: Some(Video3DFormat::HalfTopAndBottom),
            container: Some("matroska".into()),
            ..Default::default()
        };
        let path = Path::new("/movies/Avatar.2009.1080p.3D.HSBS.mkv");
        resolve_item_3d_format(path, MediaKind::Movie, &mut probe);
        assert_eq!(
            probe.video_3d_format,
            Some(Video3DFormat::HalfTopAndBottom),
            "container metadata (top_bottom) must win over conflicting filename token (HSBS)"
        );
    }

    #[test]
    fn resolve_3d_format_explicit_fsbs_overrides_heuristic_aspect_ratio() {
        let mut probe = pharos_core::MediaProbe {
            width: Some(1920),
            height: Some(1080),
            video_3d_format: Some(Video3DFormat::HalfSideBySide),
            container: Some("matroska".into()),
            ..Default::default()
        };
        let path = Path::new("/movies/Avatar.2009.1080p.3D.FSBS.mkv");
        resolve_item_3d_format(path, MediaKind::Movie, &mut probe);
        assert_eq!(
            probe.video_3d_format,
            Some(Video3DFormat::FullSideBySide),
            "explicit FSBS filename token must override anamorphic aspect-ratio heuristic on same axis"
        );
    }

    #[test]
    fn resolve_3d_format_emits_telemetry_counter() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut probe = pharos_core::MediaProbe {
            width: Some(1920),
            height: Some(1080),
            video_3d_format: Some(Video3DFormat::HalfSideBySide),
            container: Some("matroska".into()),
            ..Default::default()
        };
        let path = Path::new("/movies/Avatar.2009.1080p.mkv");
        resolve_item_3d_format(path, MediaKind::Movie, &mut probe);

        let snapshot = snapshotter.snapshot();
        let found = snapshot.into_vec().into_iter().any(|(ck, _, _, _)| {
            let k = ck.key();
            if k.name() != "pharos_3d_detect_total" {
                return false;
            }
            let labels: Vec<String> = k
                .labels()
                .map(|l| format!("{}={}", l.key(), l.value()))
                .collect();
            labels.contains(&"format=HalfSideBySide".to_string())
                && labels.contains(&"source=probe_tag".to_string())
                && labels.contains(&"media_kind=movie".to_string())
        });
        assert!(
            found,
            "pharos_3d_detect_total metric must be emitted with format, source, and media_kind labels"
        );
    }

    #[tokio::test]
    async fn scanner_probe_one_integrates_3d_detection_for_movie_and_episode() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "Avatar.2009.1080p.3D.HSBS.mkv", b"fake").await;
        write_file(td.path(), "Show.S01E01.1080p.3D.HTAB.mkv", b"fake").await;

        let s = FsScanner::new(FakeProber::default());
        let store = MemStore::default();
        s.scan_into(td.path(), &store).await.unwrap();

        let movie_item = item_by_path(&store, &td.path().join("Avatar.2009.1080p.3D.HSBS.mkv"));
        assert_eq!(
            movie_item.probe.video_3d_format,
            Some(Video3DFormat::HalfSideBySide)
        );

        let ep_item = item_by_path(&store, &td.path().join("Show.S01E01.1080p.3D.HTAB.mkv"));
        assert_eq!(
            ep_item.probe.video_3d_format,
            Some(Video3DFormat::HalfTopAndBottom)
        );
    }

    /// B1016 / V975 — a video under a show folder (one holding `tvshow.nfo`)
    /// is an episode of that show even when its name has no `S01E02` and no
    /// season folder sits between: loose absolute-numbered files, a sub-folder
    /// named for a later season, an extras folder. Each was stored as a film
    /// and matched to whatever its sidecar NFO said.
    #[tokio::test]
    async fn a_video_under_a_show_folder_is_an_episode_of_that_show() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write_file(root, "TV/Show D/tvshow.nfo", b"<tvshow/>").await;
        touch(
            root,
            "TV/Show D/[a-s]_show_d_-_063_-_the_old_second__rs2_[1AE8566B].mkv",
        )
        .await;
        write_file(root, "TV/SAO II [Season 2] [480p]/tvshow.nfo", b"<tvshow/>").await;
        touch(
            root,
            "TV/SAO II [Season 2] [480p]/SAO II/SAO II - 14 L@mBerT.mp4",
        )
        .await;
        touch(
            root,
            "TV/SAO II [Season 2] [480p]/SAO II/SAO II - 14.5 L@mBerT.mp4",
        )
        .await;
        write_file(root, "TV/Chuck/tvshow.nfo", b"<tvshow/>").await;
        touch(
            root,
            "TV/Chuck/Chuck Extras/CHUCK Season 2 DVD Bloopers [www[1].keepvid.com].mp4",
        )
        .await;
        touch(root, "TV/No Nfo/loose - 07 - title.mkv").await;
        touch(root, "Movies/Heat (1995)/Heat (1995).mkv").await;

        let items = FsScanner::new(FakeProber::default())
            .scan(root)
            .await
            .unwrap();
        let find = |needle: &str| {
            items
                .iter()
                .find(|i| i.path.to_string_lossy().contains(needle))
                .unwrap_or_else(|| panic!("{needle} not scanned"))
        };
        let shape = |needle: &str| {
            let item = find(needle);
            let series = item.series.as_ref();
            (
                item.kind,
                series.map(|s| s.series_name.clone()),
                series.and_then(|s| s.season_number),
                series.and_then(|s| s.episode_number),
            )
        };
        let episode = |name: &str, season, episode| {
            (MediaKind::Episode, Some(name.to_string()), season, episode)
        };

        assert_eq!(shape("_063_"), episode("Show D", Some(1), Some(63)));
        assert_eq!(
            shape("SAO II - 14 "),
            episode("SAO II [Season 2] [480p]", Some(1), Some(14))
        );
        assert_eq!(
            shape("SAO II - 14.5"),
            episode("SAO II [Season 2] [480p]", None, None),
            "a half-numbered recap is not episode 14"
        );
        assert_eq!(shape("Bloopers"), episode("Chuck", Some(0), None));
        assert_eq!(
            find("loose - 07").kind,
            MediaKind::Movie,
            "no tvshow.nfo, no season folder"
        );
        assert_eq!(find("Heat (1995)").kind, MediaKind::Movie);
    }

    /// B1010 / V969 — a folder holding Jellyfin's `.ignore` is left out of
    /// the library: `To Sort/` held unsorted disc rips listed as films named
    /// `title_t00`. Marking it after a scan sweeps the rows it gave.
    #[tokio::test]
    async fn a_folder_holding_dot_ignore_is_left_out() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = TempDir::new().unwrap();
        touch(td.path(), "To Sort/SG1_S9V2_E/title_t00.mkv").await;
        touch(td.path(), "Movies/Heat (1995)/Heat (1995).mkv").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        let first = s.scan_into(td.path(), &store).await.unwrap();
        assert_eq!(first.added.len(), 2);

        touch(td.path(), "To Sort/.ignore").await;
        let second = s.scan_into(td.path(), &store).await.unwrap();
        let rip = stable_id(&td.path().join("To Sort/SG1_S9V2_E/title_t00.mkv"));
        assert_eq!(second.removed, [rip], "the marked folder's row is swept");
        assert!(store
            .get(stable_id(
                &td.path().join("Movies/Heat (1995)/Heat (1995).mkv")
            ))
            .await
            .is_ok());
        assert!(in_ignored_folder(
            &td.path().join("To Sort/SG1_S9V2_E/title_t00.mkv"),
            td.path()
        ));
        assert!(!in_ignored_folder(
            &td.path().join("Movies/Heat (1995)/Heat (1995).mkv"),
            td.path()
        ));

        let counted: u64 = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter_map(|(ck, _, _, v)| match v {
                metrics_util::debugging::DebugValue::Counter(n)
                    if ck.key().name() == "pharos_scan_ignored_folders_total" =>
                {
                    Some(n)
                }
                _ => None,
            })
            .sum();
        assert_eq!(counted, 1, "one marked folder on the second walk");
    }

    /// B1023 / V982, B1024 / V983 — a film row the show-folder rule now calls
    /// an episode is re-probed though its file is unchanged, and the film's
    /// online match on it is reset so the episode is matched afresh.
    #[tokio::test]
    async fn an_unchanged_film_under_a_new_show_folder_becomes_an_episode() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = TempDir::new().unwrap();
        let file = "TV/Show D/[a-s]_show_d_-_063_-_the_old_second.mkv";
        touch(td.path(), file).await;
        touch(td.path(), "Movies/Heat (1995)/Heat (1995).mkv").await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        s.scan_into(td.path(), &store).await.unwrap();
        let id = stable_id(&td.path().join(file));
        assert_eq!(store.get(id).await.unwrap().kind, MediaKind::Movie);
        store
            .set_item_match(id, "tmdb", "1234", "nfo_id", None, 100)
            .await
            .unwrap();

        touch(td.path(), "TV/Show D/tvshow.nfo").await;
        let out = s.scan_into(td.path(), &store).await.unwrap();

        let item = store.get(id).await.unwrap();
        assert_eq!(item.kind, MediaKind::Episode);
        assert_eq!(item.series.map(|s| s.episode_number), Some(Some(63)));
        assert_eq!(item.match_source.as_deref(), Some("none"));
        assert_eq!(item.match_external_id.as_deref(), Some(""));
        assert_eq!(
            out.skipped, 1,
            "the unchanged film beside it is still skipped"
        );

        let counter = |name: &str, reason: &str| -> u64 {
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .filter_map(|(ck, _, _, v)| match v {
                    metrics_util::debugging::DebugValue::Counter(n)
                        if ck.key().name() == name
                            && ck
                                .key()
                                .labels()
                                .any(|l| l.key() == "reason" && l.value() == reason) =>
                    {
                        Some(n)
                    }
                    _ => None,
                })
                .sum()
        };
        assert_eq!(counter("pharos_scan_reprobe_total", "kind_changed"), 1);
        assert_eq!(counter("pharos_scan_match_reset_total", "kind_changed"), 1);
    }

    /// B1024 / V983 — a watched write that turns a film row into an episode
    /// resets its online match too, as the full scan does.
    #[tokio::test]
    async fn a_watched_film_turned_episode_loses_its_film_match() {
        let td = TempDir::new().unwrap();
        let file = "TV/Show D/[a-s]_show_d_-_063_-_the_old_second.mkv";
        touch(td.path(), file).await;
        let store = MemStore::default();
        let s = FsScanner::new(FakeProber::default());
        s.scan_into(td.path(), &store).await.unwrap();
        let id = stable_id(&td.path().join(file));
        store
            .set_item_match(id, "tmdb", "1234", "nfo_id", None, 100)
            .await
            .unwrap();

        touch(td.path(), "TV/Show D/tvshow.nfo").await;
        forget_show_nfos();
        // A modify event: the file's signature changes, so it is re-probed.
        tokio::fs::write(td.path().join(file), b"remuxed")
            .await
            .unwrap();
        let scan_id = store.begin_scan(td.path()).await.unwrap();
        s.update_path(&td.path().join(file), &store, scan_id)
            .await
            .unwrap();

        let item = store.get(id).await.unwrap();
        assert_eq!(item.kind, MediaKind::Episode);
        assert_eq!(item.match_source.as_deref(), Some("none"));
        assert_eq!(item.match_external_id.as_deref(), Some(""));
    }

    /// B1024 / V983 — which stored matches a re-probe resets.
    #[test]
    fn a_match_that_no_longer_fits_its_row_is_stale() {
        let film = |source: &str, tmdb: Option<&str>| {
            let mut item = MediaItem {
                kind: MediaKind::Movie,
                match_provider: Some("tmdb".into()),
                match_external_id: Some("8587".into()),
                match_source: Some(source.into()),
                ..Default::default()
            };
            item.metadata.provider_ids.tmdb = tmdb.map(Into::into);
            item
        };
        let now = |kind, tmdb: Option<&str>| {
            let mut item = MediaItem {
                kind,
                ..Default::default()
            };
            item.metadata.provider_ids.tmdb = tmdb.map(Into::into);
            item
        };
        let reason = |prior: &MediaItem, now: &MediaItem| stale_match(prior, now).map(|(r, _)| r);
        assert_eq!(
            reason(&film("nfo_id", Some("8587")), &now(MediaKind::Movie, None)),
            Some("nfo_id_gone")
        );
        assert_eq!(
            reason(
                &film("nfo_id", Some("8587")),
                &now(MediaKind::Movie, Some("8587"))
            ),
            None
        );
        assert_eq!(
            reason(&film("search", None), &now(MediaKind::Movie, None)),
            None,
            "a searched match owes nothing to the NFO"
        );
        assert_eq!(
            reason(&film("manual", None), &now(MediaKind::Episode, None)),
            Some("kind_changed")
        );
        assert_eq!(
            reason(&now(MediaKind::Movie, None), &now(MediaKind::Episode, None)),
            None
        );
    }

    /// V961, V969, V972 — a watch event never brings in what the walk skips.
    #[test]
    fn a_watch_event_skips_what_the_walk_skips() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("To Sort/SG1")).unwrap();
        std::fs::write(root.join("To Sort/.ignore"), b"").unwrap();
        for skipped in [
            "To Sort/SG1/title_t00.mkv",
            "Heat (1995)/Sample/heat.sample.mkv",
            "X/sample-The Last Starfighter (1984).mkv",
            "X/ETRG.mp4",
        ] {
            assert!(walk_skips(&root.join(skipped), root), "{skipped}");
        }
        assert!(!walk_skips(&root.join("Heat (1995)/Heat (1995).mkv"), root));
    }

    #[test]
    fn a_marker_that_reshapes_indexed_rows_is_told_apart() {
        for marker in ["/lib/To Sort/.ignore", "/lib/TV/Show D/tvshow.nfo"] {
            assert!(reshapes_indexed_rows(Path::new(marker)), "{marker}");
        }
        for other in [
            "/lib/To Sort/x.ignore",
            "/lib/.ignore/x.mkv",
            "/lib/To Sort",
            "/lib/Heat (1995)/movie.nfo",
        ] {
            assert!(!reshapes_indexed_rows(Path::new(other)), "{other}");
        }
    }
}
