//! Spec 033 §7 (V896) — an admin's hand fixes of which show a folder or file
//! is: pick or confirm a show's match, move files to another show, keep a
//! folder together, or remove a fix. Fixes are `admin` rows in
//! `series_overrides`, so a rescan cannot undo them; each one re-plans its
//! folder at once and rebuilds the episodes it moved.

use std::path::PathBuf;
use std::sync::Arc;

use pharos_core::{
    DomainError, GenreStore, MediaId, MediaStore, MetadataTranslationStore, OverrideSource,
    PersonStore, SeriesIdentityStore, SeriesMetadataStore, SeriesOverride,
};
use pharos_scanner::series_split::{self as split, RepairMode};
use tokio::sync::Semaphore;

use crate::config::MetadataConfig;
use crate::metadata_backfill::{pin_series_by_hand, rebuild_now};
use crate::online_enrich::OnlineEnricher;
use pharos_cache::ImageCache;

/// What an admin did. A metric label (`pharos_series_override_total{action}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandFix {
    PickShow,
    Confirm,
    MoveFiles,
    KeepTogether,
    Remove,
}

impl HandFix {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::PickShow,
        Self::Confirm,
        Self::MoveFiles,
        Self::KeepTogether,
        Self::Remove,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PickShow => "pick_show",
            Self::Confirm => "confirm",
            Self::MoveFiles => "move_files",
            Self::KeepTogether => "keep_together",
            Self::Remove => "remove",
        }
    }
}

fn count(action: HandFix) {
    metrics::counter!("pharos_series_override_total", "action" => action.label()).increment(1);
}

/// Why a hand fix was not made.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HandFixError {
    /// The request names nothing that exists, or is not a fix that can be
    /// made: the message says which value.
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    /// The provider did not answer with the picked show's record.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Store(#[from] DomainError),
}

/// Where a moved file goes: an existing show's key, or a new group of its
/// folder named by year.
#[derive(Debug, Clone, Default)]
pub(crate) struct MoveTarget {
    pub key: Option<String>,
    pub name: Option<String>,
    pub year: Option<u32>,
}

/// What a hand fix changed, for its answer and log line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HandFixDone {
    /// Paths whose series key moved.
    pub moved: Vec<String>,
    /// Episodes marked to be rebuilt against their show's match, which
    /// [`rebuild_paced`] does after the answer.
    pub rebuild: Vec<MediaId>,
}

/// Everything a hand fix needs: the store, the providers for a pick and the
/// rebuild, and who is making it.
pub(crate) struct HandFixer<'a, S, Tm, Tv> {
    pub store: &'a S,
    pub bg_io: &'a Arc<Semaphore>,
    /// A pick needs it to store the show's art.
    pub cache: Option<&'a ImageCache>,
    pub tmdb: Option<&'a Tm>,
    pub tvdb: Option<&'a Tv>,
    /// The configured repair mode: a fix's own rows are written in either,
    /// the scanner's own changes to the folder only under `apply`.
    pub mode: RepairMode,
    pub admin: &'a str,
    pub now: i64,
}

impl<S, Tm, Tv> HandFixer<'_, S, Tm, Tv>
where
    S: MediaStore
        + GenreStore
        + PersonStore
        + MetadataTranslationStore
        + SeriesMetadataStore
        + SeriesIdentityStore,
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
{
    /// Pin `series_key` to the provider's show `external_id`, or with
    /// `pick: None` confirm its stored match, and rebuild its episodes when
    /// the match moved.
    pub(crate) async fn pin(
        &self,
        series_key: &str,
        pick: Option<(&str, &str)>,
    ) -> Result<HandFixDone, HandFixError> {
        let action = if pick.is_some() {
            HandFix::PickShow
        } else {
            HandFix::Confirm
        };
        if let Some((provider, _)) = pick {
            if provider != "tvdb" && provider != "tmdb" {
                return Err(HandFixError::Invalid(format!(
                    "provider {provider:?} is not tvdb or tmdb"
                )));
            }
        }
        let episodes = self.store.series_episodes(series_key).await?;
        if episodes.is_empty() {
            return Err(HandFixError::NotFound(format!(
                "no episodes under series key {series_key:?}"
            )));
        }
        let Some(cache) = self.cache else {
            return Err(HandFixError::Unavailable(
                "no image cache is configured, so no show record can be stored".into(),
            ));
        };
        let name = series_name(series_key);
        let pinned = pin_series_by_hand(
            self.store, self.bg_io, cache, self.tmdb, self.tvdb, series_key, &name, pick, self.now,
        )
        .await?;
        let Some(pinned) = pinned else {
            return Err(match pick {
                Some((provider, id)) => HandFixError::Unavailable(format!(
                    "{provider} did not return show {id}; nothing written"
                )),
                None => HandFixError::NotFound(format!(
                    "series key {series_key:?} has no stored match to confirm"
                )),
            });
        };
        let ids: Vec<MediaId> = episodes.iter().map(|e| e.id).collect();
        let rebuild = if pinned.old.as_deref() == Some(pinned.new.as_str()) {
            Vec::new()
        } else {
            self.mark(ids.clone()).await?
        };
        count(action);
        tracing::info!(
            admin = self.admin,
            action = action.label(),
            series_key,
            old = pinned.old.as_deref().unwrap_or("none"),
            new = %pinned.new,
            episodes = ids.len(),
            to_rebuild = rebuild.len(),
            "series hand fix"
        );
        Ok(HandFixDone {
            moved: Vec::new(),
            rebuild,
        })
    }

    /// Send `paths` of `folder` to `target`, clearing each moved episode's
    /// own manual match so it is rebuilt against its new show.
    pub(crate) async fn move_files(
        &self,
        folder: &str,
        paths: &[String],
        target: MoveTarget,
    ) -> Result<HandFixDone, HandFixError> {
        if paths.is_empty() {
            return Err(HandFixError::Invalid("no paths to move".into()));
        }
        let target_key = match (&target.key, target.year) {
            (Some(key), _) if !key.is_empty() => key.clone(),
            (_, Some(year)) => split::split_key(folder, year),
            _ => {
                return Err(HandFixError::Invalid(
                    "a move needs a target key or a year".into(),
                ))
            }
        };
        let rows = self.folder_rows(folder).await?;
        if let Some(stray) = paths.iter().find(|p| !rows.iter().any(|r| &r.path == *p)) {
            return Err(HandFixError::Invalid(format!(
                "{stray:?} is not an episode of folder {folder:?}"
            )));
        }
        let old_keys: Vec<String> = rows
            .iter()
            .filter(|r| paths.contains(&r.path))
            .map(|r| r.series_key.clone().unwrap_or_else(|| folder.to_string()))
            .collect();
        for path in paths {
            self.store
                .replace_admin_override(SeriesOverride {
                    id: 0,
                    series_folder: folder.to_string(),
                    path: Some(path.clone()),
                    target_key: target_key.clone(),
                    target_name: target.name.clone(),
                    target_year: target.year,
                    source: OverrideSource::Admin,
                    created_by: Some(self.admin.to_string()),
                    created_at: self.now,
                })
                .await?;
        }
        let moved = self.replan(folder).await?;
        let ids = self.ids_of(folder, &moved).await?;
        let cleared = self.store.clear_manual_matches(&ids).await?;
        let rebuild = self.mark(ids).await?;
        count(HandFix::MoveFiles);
        tracing::info!(
            admin = self.admin,
            action = HandFix::MoveFiles.label(),
            folder,
            paths = ?paths,
            old_keys = ?dedup(old_keys),
            new_key = %target_key,
            moved = moved.len(),
            cleared_manual = ?cleared,
            to_rebuild = rebuild.len(),
            "series hand fix"
        );
        Ok(HandFixDone { moved, rebuild })
    }

    /// Keep `folder` one show whatever years its files state.
    pub(crate) async fn keep_together(&self, folder: &str) -> Result<HandFixDone, HandFixError> {
        if self.folder_rows(folder).await?.is_empty() {
            return Err(HandFixError::NotFound(format!(
                "no episodes in folder {folder:?}"
            )));
        }
        self.store
            .replace_admin_override(SeriesOverride::keep_together(
                folder,
                Some(self.admin.to_string()),
                self.now,
            ))
            .await?;
        let moved = self.replan(folder).await?;
        let ids = self.ids_of(folder, &moved).await?;
        let rebuild = self.mark(ids).await?;
        count(HandFix::KeepTogether);
        tracing::info!(
            admin = self.admin,
            action = HandFix::KeepTogether.label(),
            folder,
            new_key = folder,
            moved = moved.len(),
            to_rebuild = rebuild.len(),
            "series hand fix"
        );
        Ok(HandFixDone { moved, rebuild })
    }

    /// Remove override `id`, returning its files to what the scanner decides.
    /// In a dry run the scanner decides nothing new, so a file the removed
    /// row had moved goes back to its folder's key.
    pub(crate) async fn remove(&self, id: i64) -> Result<HandFixDone, HandFixError> {
        let Some(row) = self.store.delete_series_override(id).await? else {
            return Err(HandFixError::NotFound(format!("no override {id}")));
        };
        let folder = row.series_folder.as_str();
        let mut moved = self.replan(folder).await?;
        if self.mode == RepairMode::DryRun {
            if let Some(path) = &row.path {
                let back =
                    self.folder_rows(folder).await?.into_iter().find(|r| {
                        &r.path == path && r.series_key.as_deref() == Some(&row.target_key)
                    });
                if let Some(back) = back {
                    self.store
                        .set_series_keys(&[pharos_core::SeriesKeyUpdate {
                            path: back.path.clone(),
                            series_key: None,
                            series_year: back.series_year,
                        }])
                        .await?;
                    moved.push(back.path);
                }
            }
        }
        let ids = self.ids_of(folder, &moved).await?;
        let rebuild = self.mark(ids).await?;
        count(HandFix::Remove);
        tracing::info!(
            admin = self.admin,
            action = HandFix::Remove.label(),
            override_id = id,
            folder,
            path = row.path.as_deref(),
            source = row.source.as_str(),
            old_key = %row.target_key,
            moved = moved.len(),
            to_rebuild = rebuild.len(),
            "series hand fix"
        );
        Ok(HandFixDone { moved, rebuild })
    }

    async fn folder_rows(
        &self,
        folder: &str,
    ) -> Result<Vec<pharos_core::SeriesIdentityRow>, HandFixError> {
        Ok(self
            .store
            .series_identity_rows(folder)
            .await?
            .into_iter()
            .filter(|r| r.series_folder.as_deref() == Some(folder))
            .collect())
    }

    /// Plan `folder` again from its stored rows and write the result under
    /// the configured mode. Returns the paths whose key moved.
    async fn replan(&self, folder: &str) -> Result<Vec<String>, HandFixError> {
        let paths: Vec<PathBuf> = self
            .folder_rows(folder)
            .await?
            .into_iter()
            .map(|r| PathBuf::from(r.path))
            .collect();
        let folders = split::folders_of(&paths);
        let ctx = split::context(self.store, &folders).await?;
        let plan = split::plan(&folders, &ctx, self.now);
        Ok(split::reconcile_folder(self.store, &plan, folder, self.mode, self.now).await?)
    }

    /// The episode ids of `paths` of `folder`, by the stored rows of their
    /// current keys.
    async fn ids_of(&self, folder: &str, paths: &[String]) -> Result<Vec<MediaId>, HandFixError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let keys = dedup(
            self.folder_rows(folder)
                .await?
                .into_iter()
                .filter(|r| paths.contains(&r.path))
                .map(|r| r.series_key.unwrap_or_else(|| folder.to_string()))
                .collect(),
        );
        let mut ids = Vec::new();
        for key in keys {
            ids.extend(
                self.store
                    .series_episodes(&key)
                    .await?
                    .into_iter()
                    .filter(|e| paths.contains(&e.path))
                    .map(|e| e.id),
            );
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Mark `ids` for a rebuild now, so a restart before [`rebuild_paced`]
    /// finishes leaves them to the background pass under `apply`.
    async fn mark(&self, ids: Vec<MediaId>) -> Result<Vec<MediaId>, HandFixError> {
        if !ids.is_empty() {
            self.store.mark_reconcile_items(&ids, self.now).await?;
        }
        Ok(ids)
    }
}

/// Gap between two episodes [`rebuild_paced`] rebuilds: the background
/// pass's spacing, so a pick on a long show is no burst at the provider.
const REBUILD_SPACING: std::time::Duration = std::time::Duration::from_millis(120);

/// Rebuild the episodes a hand fix marked, one at a time and spaced, after
/// the fix has answered: a pick on a show of 1000 episodes takes minutes.
/// One whose show has no match yet, or whose record did not arrive, stays
/// marked. Logs one line when done.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rebuild_paced<S, Tm, Tv>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    cfg: &MetadataConfig,
    ids: &[MediaId],
    admin: &str,
) where
    S: MediaStore
        + GenreStore
        + PersonStore
        + MetadataTranslationStore
        + SeriesMetadataStore
        + SeriesIdentityStore,
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
{
    let (mut rebuilt, mut deferred, mut failed) = (0usize, 0usize, 0usize);
    for (i, &id) in ids.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(REBUILD_SPACING).await;
        }
        let now = crate::metadata_backfill::now_secs();
        match rebuild_now(store, bg_io, cache, tmdb, tvdb, cfg, &[id], now).await {
            Ok(0) => deferred += 1,
            Ok(_) => rebuilt += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(error = %e, media.id = id, admin, "series hand fix: episode rebuild failed");
            }
        }
    }
    tracing::info!(
        admin,
        episodes = ids.len(),
        rebuilt,
        deferred,
        failed,
        "series hand fix: episodes rebuilt"
    );
}

/// A show's display name from its key: the folder's last segment, without a
/// split's year.
pub(crate) fn series_name(series_key: &str) -> String {
    let folder = series_key
        .split(split::SPLIT_KEY_SEPARATOR)
        .next()
        .unwrap_or(series_key);
    folder.rsplit('/').next().unwrap_or(folder).to_string()
}

fn dedup(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::tmdb::TmdbEnricher;
    use crate::tvdb::{ReqwestTransport, TvdbEnricher};
    use pharos_core::{MediaItem, MediaKind, SeriesInfo};
    use pharos_store_sqlx::sqlite::SqliteStore;

    const FLASH: &str = "/tv/The Flash";

    fn path(name: &str) -> String {
        format!("{FLASH}/Season 1/{name}")
    }

    async fn flash() -> SqliteStore {
        let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
        for (id, name, episode) in [
            (1, "The Flash - S01E01 - Pilot.mkv", 1),
            (2, "The Flash - S01E02 - Out of Control.mkv", 2),
            (3, "The Flash - S01E03 - City of Heroes.mkv", 3),
        ] {
            s.put(MediaItem {
                id,
                path: path(name).into(),
                title: name.into(),
                kind: MediaKind::Episode,
                series: Some(SeriesInfo {
                    series_name: "The Flash".into(),
                    season_number: Some(1),
                    episode_number: Some(episode),
                    series_folder: Some(FLASH.into()),
                    series_year: None,
                    series_key: None,
                }),
                ..MediaItem::default()
            })
            .await
            .unwrap();
        }
        s
    }

    fn fixer<'a>(
        s: &'a SqliteStore,
        bg_io: &'a Arc<Semaphore>,
        mode: RepairMode,
    ) -> HandFixer<'a, SqliteStore, TmdbEnricher, TvdbEnricher<ReqwestTransport>> {
        HandFixer {
            store: s,
            bg_io,
            cache: None,
            tmdb: None,
            tvdb: None,
            mode,
            admin: "ali",
            now: 100,
        }
    }

    async fn keys(s: &SqliteStore) -> Vec<(String, Option<String>)> {
        let mut rows: Vec<_> = s
            .series_identity_rows(FLASH)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.path.rsplit('/').next().unwrap().to_string(), r.series_key))
            .collect();
        rows.sort();
        rows
    }

    fn counter(snap: &pharos_test_metrics::CumulativeSnapshotter, action: &str) -> u64 {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_series_override_total")
            .filter(|(k, ..)| {
                k.key()
                    .labels()
                    .any(|l| l.key() == "action" && l.value() == action)
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(c) => c,
                _ => 0,
            })
            .sum()
    }

    #[derive(Clone, Default)]
    struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> LogBuf {
            self.clone()
        }
    }

    fn fix_line(logs: &LogBuf) -> String {
        String::from_utf8_lossy(&logs.0.lock().unwrap())
            .lines()
            .rfind(|l| l.contains("series hand fix"))
            .unwrap_or_default()
            .to_string()
    }

    /// V896 — a move is an admin row, written in a dry run, counted and
    /// logged with the admin, the paths and both keys; it clears the moved
    /// episode's own manual match, and planning the folder again (what a
    /// rescan does) leaves it where the admin put it.
    #[tokio::test(flavor = "current_thread")]
    async fn moving_a_file_survives_a_rescan_and_is_counted_and_logged() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let logs = LogBuf::default();
        let _log = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        let s = flash().await;
        s.set_item_match(3, "tvdb", "279121", "manual", None, 1)
            .await
            .unwrap();
        let bg = Arc::new(Semaphore::new(1));
        let f = fixer(&s, &bg, RepairMode::DryRun);
        let moved = path("The Flash - S01E03 - City of Heroes.mkv");

        let done = f
            .move_files(
                FLASH,
                std::slice::from_ref(&moved),
                MoveTarget {
                    year: Some(2014),
                    ..MoveTarget::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(done.moved, std::slice::from_ref(&moved));
        let new_key = split::split_key(FLASH, 2014);
        let want = vec![
            ("The Flash - S01E01 - Pilot.mkv".to_string(), None),
            ("The Flash - S01E02 - Out of Control.mkv".to_string(), None),
            (
                "The Flash - S01E03 - City of Heroes.mkv".to_string(),
                Some(new_key.clone()),
            ),
        ];
        assert_eq!(keys(&s).await, want);
        assert_eq!(s.get(3).await.unwrap().match_source, None);
        assert_eq!(counter(&snap, "move_files"), 1);
        let line = fix_line(&logs);
        for field in [
            "admin=\"ali\"",
            "action=\"move_files\"",
            "City of Heroes.mkv",
            "old_keys=[\"/tv/The Flash\"]",
            "cleared_manual=[(3, \"279121\")]",
        ] {
            assert!(line.contains(field), "{field} missing from {line}");
        }
        assert!(line.contains(&format!("new_key={new_key}")), "{line}");

        for mode in [RepairMode::DryRun, RepairMode::Apply] {
            fixer(&s, &bg, mode).replan(FLASH).await.unwrap();
            assert_eq!(keys(&s).await, want, "{mode:?}");
        }
    }

    /// Removing a move returns the file to its folder's key in a dry run;
    /// keeping a folder together rejoins a file a move had split off.
    #[tokio::test(flavor = "current_thread")]
    async fn removing_a_move_or_keeping_together_returns_the_file() {
        let s = flash().await;
        let bg = Arc::new(Semaphore::new(1));
        let f = fixer(&s, &bg, RepairMode::DryRun);
        let moved = path("The Flash - S01E02 - Out of Control.mkv");
        let target = MoveTarget {
            year: Some(1990),
            ..MoveTarget::default()
        };
        f.move_files(FLASH, std::slice::from_ref(&moved), target.clone())
            .await
            .unwrap();
        let id = s.all_series_overrides().await.unwrap()[0].id;

        let done = f.remove(id).await.unwrap();

        assert_eq!(done.moved, std::slice::from_ref(&moved));
        assert!(keys(&s).await.iter().all(|(_, k)| k.is_none()));
        assert!(s.all_series_overrides().await.unwrap().is_empty());

        f.move_files(FLASH, std::slice::from_ref(&moved), target)
            .await
            .unwrap();
        let apply = fixer(&s, &bg, RepairMode::Apply);
        apply.keep_together(FLASH).await.unwrap();
        // The admin's own per-path row still places its file.
        assert_eq!(
            keys(&s).await[1].1.as_deref(),
            Some(split::split_key(FLASH, 1990).as_str())
        );
        let rows = s.all_series_overrides().await.unwrap();
        assert!(rows.iter().any(SeriesOverride::keeps_together), "{rows:?}");
    }

    /// Each refusal names the value it refused, and nothing is written.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hand_fix_that_cannot_be_made_names_why() {
        let s = flash().await;
        let bg = Arc::new(Semaphore::new(1));
        let f = fixer(&s, &bg, RepairMode::DryRun);
        let year = MoveTarget {
            year: Some(2014),
            ..MoveTarget::default()
        };
        let stray = "/tv/Other/S01E01.mkv".to_string();
        let err = f
            .move_files(FLASH, &[stray], year.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HandFixError::Invalid(m) if m.contains("/tv/Other/S01E01.mkv")),
            "{err:?}"
        );
        let err = f
            .move_files(
                FLASH,
                &[path("The Flash - S01E01 - Pilot.mkv")],
                MoveTarget::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HandFixError::Invalid(_)), "{err:?}");
        assert!(matches!(f.remove(42).await, Err(HandFixError::NotFound(m)) if m.contains("42")));
        assert!(matches!(
            f.keep_together("/tv/Nothing").await,
            Err(HandFixError::NotFound(_))
        ));
        assert!(matches!(
            f.pin(FLASH, Some(("imdb", "tt1"))).await,
            Err(HandFixError::Invalid(m)) if m.contains("imdb")
        ));
        assert!(s.all_series_overrides().await.unwrap().is_empty());
    }

    #[test]
    fn hand_fix_labels_are_distinct() {
        let labels: std::collections::BTreeSet<&str> =
            HandFix::ALL.iter().map(|a| a.label()).collect();
        assert_eq!(labels.len(), HandFix::ALL.len());
        assert_eq!(
            labels,
            [
                "confirm",
                "keep_together",
                "move_files",
                "pick_show",
                "remove"
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn a_split_keys_name_drops_its_year() {
        assert_eq!(series_name("/tv/The Flash"), "The Flash");
        assert_eq!(
            series_name(&split::split_key("/tv/The Flash", 2014)),
            "The Flash"
        );
    }
}
