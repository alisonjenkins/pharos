//! Repairs artwork rows whose file is gone (B1180, V1140).
//!
//! An `artwork` row can outlive the file it names, in two ways seen in
//! practice. A row stored before B571 holds the media server's container path
//! (for example `/media/TV/<show>/backdrop.jpg`) while pharos mounts the
//! library somewhere else, so the same file sits in the item's own folder under
//! the same basename. Or a sidecar (`folder.jpg`) was deleted. Nothing ever
//! repaired either row: the scanner re-resolves artwork only when a file is
//! re-probed, and `persist_artwork` only upserts. So the image route warned and
//! fell back to a video frame, `has_primary_art` stayed true, and `enrich_one`
//! kept treating the dead sidecar as curated art, so online art never replaced
//! it.
//!
//! The sweep reads every servable artwork row and checks its file. For a dead
//! `local` row it relinks to a same-named file in the item's folder or one of
//! its three nearest ancestors, when one exists. Otherwise it drops the row and
//! re-admits the item for online art, unless the match is `manual`. Provider
//! cached rows (tmdb, tvdb, musicbrainz) have no sibling, so they are only ever
//! dropped. A row is only judged dead on a definite "not found", and only
//! repaired while the item's own media file is visible: when the media is
//! missing too, the share is most likely down, and dropping then would strip
//! the whole library of its art.
//!
//! Signals: `pharos_artwork_stale_total{role,outcome}` per dead row,
//! `pharos_artwork_stale_rows` (dead rows found in the last pass), one info line
//! per dead row naming its locator and outcome, and one info line per pass.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pharos_core::{ArtworkRow, DomainResult, MediaId, MediaStore};
use tokio::sync::Semaphore;

use crate::bg_io::BgPermit;
use crate::state::Stores;

/// Rows read per page. Each page takes one background-IO permit.
const PAGE: i64 = 500;
/// How far above the media file a same-named sidecar is looked for: the
/// item's folder, then its parent, then its grandparent.
const ANCESTORS: usize = 3;
/// Delay before the first pass after boot, so startup I/O settles first.
const FIRST_PASS: Duration = Duration::from_secs(5 * 60);
/// Interval between passes once one has run.
const PASS_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Follower re-check cadence, so leadership handoff is picked up promptly.
const FOLLOWER_POLL: Duration = Duration::from_secs(30);

/// What the sweep did with one dead row. `label()` is the metric's `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Pointed at a same-named file in the item's folder.
    Relinked,
    /// No file to point at; the row was removed and the item re-admitted.
    Dropped,
    /// Found dead but not repaired: detect-only mode, or a non-repair pass.
    Missing,
    /// The item the row belongs to no longer exists.
    Orphan,
    /// The item's own media file could not be seen either, so the share is
    /// probably down rather than the artwork deleted: left untouched.
    Unreachable,
}

impl Outcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Outcome::Relinked => "relinked",
            Outcome::Dropped => "dropped",
            Outcome::Missing => "missing",
            Outcome::Orphan => "orphan",
            Outcome::Unreachable => "unreachable",
        }
    }
}

/// Counts for one pass. `checked` is every servable row read; the rest count
/// the dead rows by outcome.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PassReport {
    pub checked: u64,
    pub relinked: u64,
    pub dropped: u64,
    pub missing: u64,
    pub orphan: u64,
    pub unreachable: u64,
}

impl PassReport {
    fn count(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Relinked => self.relinked += 1,
            Outcome::Dropped => self.dropped += 1,
            Outcome::Missing => self.missing += 1,
            Outcome::Orphan => self.orphan += 1,
            Outcome::Unreachable => self.unreachable += 1,
        }
    }

    fn dead(&self) -> u64 {
        self.relinked + self.dropped + self.missing + self.orphan + self.unreachable
    }
}

/// Walks every servable artwork row and repairs the dead ones. With
/// `repair == false` nothing is written: dead rows are only counted.
pub(crate) async fn run_pass<S: MediaStore>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    repair: bool,
) -> DomainResult<PassReport> {
    let mut report = PassReport::default();
    let mut cursor: Option<(MediaId, String)> = None;
    loop {
        let page = {
            let _permit = BgPermit::acquire(bg_io).await;
            store.servable_artwork_page(cursor.clone(), PAGE).await?
        };
        // The cursor is the last key read, taken before any row is changed.
        // A relink keeps its key and a drop removes only rows behind the
        // cursor, so the next page starts exactly after this one.
        let Some(last) = page.last() else { break };
        let next = (last.item_id, last.role.clone());
        let full = i64::try_from(page.len()).is_ok_and(|n| n >= PAGE);
        for row in &page {
            report.checked += 1;
            if let Some(outcome) = repair_row(store, row, repair).await? {
                report.count(outcome);
            }
        }
        if !full {
            break;
        }
        cursor = Some(next);
    }
    metrics::gauge!("pharos_artwork_stale_rows").set(report.dead() as f64);
    tracing::info!(
        checked = report.checked,
        relinked = report.relinked,
        dropped = report.dropped,
        missing = report.missing,
        orphan = report.orphan,
        unreachable = report.unreachable,
        "stale artwork sweep: pass complete"
    );
    Ok(report)
}

/// Checks one row. `None` means its file is there. Otherwise the dead row is
/// repaired (or only detected, when `repair` is false) and its outcome returned.
async fn repair_row<S: MediaStore>(
    store: &S,
    row: &ArtworkRow,
    repair: bool,
) -> DomainResult<Option<Outcome>> {
    // Only a definite "not there" counts as dead: an I/O error (a stale NFS
    // handle, a permission blip) says nothing about the file.
    if !matches!(tokio::fs::try_exists(&row.locator).await, Ok(false)) {
        return Ok(None);
    }
    let Ok(item) = store.get(row.item_id).await else {
        tracing::info!(
            media.id = row.item_id,
            role = %row.role,
            source = %row.source,
            locator = %row.locator,
            outcome = Outcome::Orphan.label(),
            "stale artwork row: its item is gone"
        );
        metrics::counter!(
            "pharos_artwork_stale_total",
            "role" => row.role.clone(),
            "outcome" => Outcome::Orphan.label()
        )
        .increment(1);
        return Ok(Some(Outcome::Orphan));
    };
    // Only a local sidecar has a sibling to relink to. A provider-cached file
    // lives in the cache, not beside the media.
    // If the media file itself is gone too, the library share is most likely
    // unmounted or stale. Dropping now would strip every item of its art and
    // queue a provider download for each, so leave the row for a later pass.
    if !matches!(tokio::fs::try_exists(&item.path).await, Ok(true)) {
        tracing::info!(
            media.id = row.item_id,
            role = %row.role,
            locator = %row.locator,
            media.path = %item.path.display(),
            outcome = Outcome::Unreachable.label(),
            "stale artwork row: media file unreachable too; left alone"
        );
        metrics::counter!(
            "pharos_artwork_stale_total",
            "role" => row.role.clone(),
            "outcome" => Outcome::Unreachable.label()
        )
        .increment(1);
        return Ok(Some(Outcome::Unreachable));
    }
    let sibling = if row.source.eq_ignore_ascii_case("local") {
        find_sibling(&item.path, &row.locator).await
    } else {
        None
    };
    let outcome = match (&sibling, repair) {
        (Some(path), true) => {
            store
                .set_artwork(row.item_id, &row.role, "local", &path.to_string_lossy())
                .await?;
            store.bump_art_version(row.item_id).await?;
            Outcome::Relinked
        }
        (None, true) => {
            store
                .remove_dead_artwork(row.item_id, &row.role, true)
                .await?;
            Outcome::Dropped
        }
        (_, false) => Outcome::Missing,
    };
    tracing::info!(
        media.id = row.item_id,
        role = %row.role,
        source = %row.source,
        locator = %row.locator,
        relinked = sibling.as_deref().map(|p| p.display().to_string()),
        outcome = outcome.label(),
        "stale artwork row: file missing on disk"
    );
    metrics::counter!(
        "pharos_artwork_stale_total",
        "role" => row.role.clone(),
        "outcome" => outcome.label()
    )
    .increment(1);
    Ok(Some(outcome))
}

/// The first regular file named like `locator`'s basename in the media file's
/// folder or one of its [`ANCESTORS`] nearest ancestors. The folder itself is
/// tried first.
async fn find_sibling(media: &Path, locator: &str) -> Option<PathBuf> {
    let name = Path::new(locator).file_name()?;
    for dir in media.ancestors().skip(1).take(ANCESTORS) {
        let candidate = dir.join(name);
        if tokio::fs::metadata(&candidate)
            .await
            .is_ok_and(|m| m.is_file())
        {
            return Some(candidate);
        }
    }
    None
}

/// Runs the sweep forever: first pass [`FIRST_PASS`] after boot, then every
/// [`PASS_EVERY`]. Only the background leader runs it; a follower re-checks
/// every [`FOLLOWER_POLL`]. A pass that relinked or dropped a row bumps
/// `artwork_epoch`, so synthesised images that memoised the old artwork are
/// rebuilt. A pass error is logged and the loop carries on.
pub fn spawn(
    stores: Stores,
    bg_io: Arc<Semaphore>,
    is_bg_leader: Arc<AtomicBool>,
    repair: bool,
    artwork_epoch: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_PASS).await;
        loop {
            if !is_bg_leader.load(Ordering::Relaxed) {
                tokio::time::sleep(FOLLOWER_POLL).await;
                continue;
            }
            match run_pass(&stores, &bg_io, repair).await {
                Ok(report) => {
                    if report.relinked + report.dropped > 0 {
                        artwork_epoch.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "stale artwork sweep: pass aborted");
                }
            }
            tokio::time::sleep(PASS_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{MediaItem, MediaKind};

    const NOW: i64 = 1_000_000;

    #[test]
    fn outcome_labels_are_distinct() {
        let labels = [
            Outcome::Relinked,
            Outcome::Dropped,
            Outcome::Missing,
            Outcome::Orphan,
            Outcome::Unreachable,
        ]
        .map(Outcome::label);
        let unique: std::collections::BTreeSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    /// A movie at `<dir>/a-film (2000)/film.mkv`, the folder the sidecars sit in.
    async fn library(dir: &Path) -> (Stores, u64, PathBuf) {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let folder = dir.join("a-film (2000)");
        std::fs::create_dir_all(&folder).unwrap();
        let media = folder.join("film.mkv");
        std::fs::write(&media, b"video").unwrap();
        stores
            .put(MediaItem {
                id: 1,
                path: media.clone(),
                title: "a film".into(),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
        (stores, 1, folder)
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"img").unwrap();
    }

    fn bg() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(4))
    }

    /// Stamp the item as matched by `source` at [`NOW`], so it is not due
    /// until a dead row's removal re-admits it.
    async fn match_item(stores: &Stores, id: u64, source: &str) {
        stores
            .set_item_match(id, "tmdb", "ext-1", source, None, NOW)
            .await
            .unwrap();
    }

    async fn due(stores: &Stores) -> Vec<u64> {
        stores
            .items_needing_match(10, NOW - 1)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect()
    }

    #[tokio::test]
    async fn a_dead_row_with_a_sibling_is_relinked() {
        let dir = tempfile::tempdir().unwrap();
        let (stores, id, folder) = library(dir.path()).await;
        touch(&folder.join("backdrop.jpg"));
        stores
            .set_artwork(id, "Backdrop", "local", "/old-library/a-show/backdrop.jpg")
            .await
            .unwrap();
        let before = stores.get(id).await.unwrap().art_version;

        let report = run_pass(&stores, &bg(), true).await.unwrap();

        assert_eq!(report.relinked, 1, "{report:?}");
        let rows = stores.artwork_for(id).await.unwrap();
        assert_eq!(
            rows,
            vec![(
                "Backdrop".to_owned(),
                "local".to_owned(),
                folder.join("backdrop.jpg").to_string_lossy().into_owned(),
            )]
        );
        assert_eq!(stores.get(id).await.unwrap().art_version, before + 1);
    }

    #[tokio::test]
    async fn a_dead_primary_with_no_sibling_is_dropped_and_readmitted() {
        let dir = tempfile::tempdir().unwrap();
        let (stores, id, folder) = library(dir.path()).await;
        stores
            .set_artwork(
                id,
                "Primary",
                "local",
                &folder.join("folder.jpg").to_string_lossy(),
            )
            .await
            .unwrap();
        match_item(&stores, id, "search").await;
        assert!(due(&stores).await.is_empty(), "matched and fresh");

        let report = run_pass(&stores, &bg(), true).await.unwrap();

        assert_eq!(report.dropped, 1, "{report:?}");
        assert!(stores.artwork_for(id).await.unwrap().is_empty());
        let item = stores.get(id).await.unwrap();
        assert!(!item.has_primary_art);
        assert_eq!(due(&stores).await, vec![id], "re-admitted for online art");
    }

    #[tokio::test]
    async fn a_dead_row_on_a_manual_match_is_dropped_but_not_readmitted() {
        let dir = tempfile::tempdir().unwrap();
        let (stores, id, folder) = library(dir.path()).await;
        stores
            .set_artwork(
                id,
                "Primary",
                "local",
                &folder.join("folder.jpg").to_string_lossy(),
            )
            .await
            .unwrap();
        match_item(&stores, id, "manual").await;

        let report = run_pass(&stores, &bg(), true).await.unwrap();

        assert_eq!(report.dropped, 1, "{report:?}");
        assert!(stores.artwork_for(id).await.unwrap().is_empty());
        assert_eq!(
            stores.get(id).await.unwrap().metadata_refreshed_at,
            Some(NOW),
            "a manual match keeps its refresh stamp"
        );
        assert!(due(&stores).await.is_empty(), "never re-admitted");
    }

    #[tokio::test]
    async fn detect_only_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (stores, id, folder) = library(dir.path()).await;
        touch(&folder.join("backdrop.jpg"));
        stores
            .set_artwork(id, "Backdrop", "local", "/old-library/a-show/backdrop.jpg")
            .await
            .unwrap();
        stores
            .set_artwork(
                id,
                "Primary",
                "local",
                &folder.join("folder.jpg").to_string_lossy(),
            )
            .await
            .unwrap();
        let rows_before = stores.artwork_for(id).await.unwrap();
        let version_before = stores.get(id).await.unwrap().art_version;

        let report = run_pass(&stores, &bg(), false).await.unwrap();

        assert_eq!(report.missing, 2, "{report:?}");
        assert_eq!(report.relinked + report.dropped, 0, "{report:?}");
        assert_eq!(stores.artwork_for(id).await.unwrap(), rows_before);
        assert_eq!(stores.get(id).await.unwrap().art_version, version_before);
    }

    #[tokio::test]
    async fn an_existing_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (stores, id, folder) = library(dir.path()).await;
        let poster = folder.join("folder.jpg");
        touch(&poster);
        let locator = poster.to_string_lossy().into_owned();
        stores
            .set_artwork(id, "Primary", "local", &locator)
            .await
            .unwrap();
        let version_before = stores.get(id).await.unwrap().art_version;

        let report = run_pass(&stores, &bg(), true).await.unwrap();

        assert_eq!(report.checked, 1, "{report:?}");
        assert_eq!(report.relinked + report.dropped + report.missing, 0);
        assert_eq!(
            stores.artwork_for(id).await.unwrap(),
            vec![("Primary".to_owned(), "local".to_owned(), locator)]
        );
        assert_eq!(stores.get(id).await.unwrap().art_version, version_before);
    }

    #[tokio::test]
    async fn a_dead_provider_row_is_dropped_never_relinked() {
        let dir = tempfile::tempdir().unwrap();
        let (stores, id, folder) = library(dir.path()).await;
        // A same-named file sits beside the media, but a provider's cache
        // entry has no sibling to relink to.
        touch(&folder.join("backdrop.jpg"));
        stores
            .set_artwork(id, "Backdrop", "tmdb", "/cache/gone/backdrop.jpg")
            .await
            .unwrap();

        let report = run_pass(&stores, &bg(), true).await.unwrap();

        assert_eq!(report.dropped, 1, "{report:?}");
        assert_eq!(report.relinked, 0, "{report:?}");
        assert!(stores.artwork_for(id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_dead_row_whose_media_is_unreachable_is_left_alone() {
        // The share dropping out makes the media file vanish along with its
        // art; nothing may be dropped or re-admitted on that evidence.
        let dir = tempfile::tempdir().unwrap();
        let (stores, id, folder) = library(dir.path()).await;
        let poster = folder.join("folder.jpg").to_string_lossy().into_owned();
        stores
            .set_artwork(id, "Primary", "local", &poster)
            .await
            .unwrap();
        match_item(&stores, id, "search").await;
        std::fs::remove_file(folder.join("film.mkv")).unwrap();

        let report = run_pass(&stores, &bg(), true).await.unwrap();

        assert_eq!(report.unreachable, 1, "{report:?}");
        assert_eq!(report.dropped, 0, "{report:?}");
        assert_eq!(
            stores.artwork_for(id).await.unwrap(),
            vec![("Primary".to_owned(), "local".to_owned(), poster)]
        );
        assert!(stores.get(id).await.unwrap().has_primary_art);
        assert!(due(&stores).await.is_empty(), "not re-admitted");
    }
}
