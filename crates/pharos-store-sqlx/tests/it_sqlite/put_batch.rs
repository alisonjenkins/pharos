//! `MediaStore::put_batch` must leave exactly what the same items put one at a
//! time leave: the row, the `title_fold`/`sort_name` columns, the language
//! index the triggers derive, and the FTS hits. Runs on SQLite always and on
//! Postgres whenever `PHAROS_TEST_POSTGRES_URL` is set (`just test-postgres`).

use pharos_core::{
    AudioTrack, BookFormat, BookMeta, DomainError, MediaId, MediaItem, MediaKind, MediaMetadata,
    MediaProbe, MediaStore, SearchQuery, SeriesInfo, SubtitleTrack,
};

/// Far above any id the other suites use, so a shared Postgres database is
/// not disturbed.
const BASE: MediaId = 9_100_000_000;

fn track(lang: &str) -> AudioTrack {
    AudioTrack {
        stream_index: 1,
        language: Some(lang.into()),
        ..Default::default()
    }
}

fn sub(lang: &str) -> SubtitleTrack {
    SubtitleTrack {
        stream_index: 2,
        language: Some(lang.into()),
        codec: Some("subrip".into()),
        title: None,
        is_default: false,
        is_forced: false,
        is_hearing_impaired: false,
    }
}

/// Items that between them touch every optional column family: tracks, series,
/// book, metadata, a non-ASCII title for `title_fold` / `sort_name`.
fn rich(id: MediaId, n: usize) -> MediaItem {
    let mut item = MediaItem {
        id,
        path: format!("/pb/{id}.mkv").into(),
        title: format!("The Édition {n} Zebra"),
        kind: MediaKind::Movie,
        created_at: Some(1_700_000_000 + n as i64),
        probe: MediaProbe {
            size_bytes: Some(1_000 + n as u64),
            width: Some(1920),
            audio_tracks: vec![track("eng"), track("jpn")],
            subtitle_tracks: vec![sub("fre")],
            ..Default::default()
        },
        metadata: MediaMetadata {
            overview: Some(format!("overview {n}")),
            community_rating: Some(7.5),
            production_locations: vec!["Japan".into()],
            original_language: Some("ja".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    match n % 3 {
        1 => {
            item.kind = MediaKind::Episode;
            item.series = Some(SeriesInfo {
                series_name: format!("Show {n}"),
                season_number: Some(2),
                episode_number: Some(n as u32),
                series_folder: Some(format!("/pb/show{n}")),
                series_year: Some(1999),
            });
        }
        2 => {
            item.kind = MediaKind::Book;
            item.path = format!("/pb/{id}.epub").into();
            item.probe.audio_tracks.clear();
            item.probe.subtitle_tracks.clear();
            item.book = Some(BookMeta {
                format: BookFormat::Epub,
                page_count: Some(300),
                author: Some("An Author".into()),
                ..Default::default()
            });
        }
        _ => {
            // 3D columns: the primary's packing and an edition's, so a bind-order
            // slip in either backend's batch path shows up as a snapshot diff.
            item.probe.video_3d_format = Some(pharos_core::Video3DFormat::FullSideBySide);
            item.probe
                .alternate_sources
                .push(pharos_core::AlternateMediaSource {
                    id: "3d-htab".into(),
                    path: format!("/pb/{id}.3d.htab.mkv").into(),
                    video_3d_format: Some(pharos_core::Video3DFormat::HalfTopAndBottom),
                    ..Default::default()
                });
        }
    }
    item
}

#[derive(Debug, PartialEq)]
struct Snap {
    item: MediaItem,
    title_fold: String,
    sort_name: String,
    langs: Vec<(String, String)>,
}

trait Inspect: MediaStore {
    async fn title_fold_sort_name(&self, id: MediaId) -> (String, String);
    async fn language_rows(&self, id: MediaId) -> Vec<(String, String)>;
    async fn delete_item(&self, id: MediaId);
    async fn count_between(&self, lo: MediaId, hi: MediaId) -> i64;
    /// How many separate transactions wrote the rows in `[lo, hi)`, where the
    /// backend can say (Postgres `xmin`). `None` otherwise.
    async fn writer_transactions(&self, lo: MediaId, hi: MediaId) -> Option<i64>;
}

async fn snap<S: Inspect>(s: &S, ids: &[MediaId]) -> Vec<Snap> {
    let mut out = Vec::new();
    for &id in ids {
        let (title_fold, sort_name) = s.title_fold_sort_name(id).await;
        out.push(Snap {
            item: s.get(id).await.unwrap(),
            title_fold,
            sort_name,
            langs: s.language_rows(id).await,
        });
    }
    out
}

async fn search_ids<S: MediaStore>(s: &S, term: &str) -> Vec<MediaId> {
    let q = SearchQuery {
        term: term.into(),
        kinds: Vec::new(),
        limit: 10_000,
        offset: 0,
    };
    let mut ids: Vec<_> = s
        .search(&q)
        .await
        .unwrap()
        .0
        .into_iter()
        .map(|i| i.id)
        .filter(|id| *id >= BASE)
        .collect();
    ids.sort_unstable();
    ids
}

/// Apply `steps` once as individual puts and once as one `put_batch` per step;
/// both must end in the same state.
async fn assert_batch_equals_singles<S: Inspect>(s: &S, steps: &[Vec<MediaItem>], what: &str) {
    let mut ids: Vec<MediaId> = steps.iter().flatten().map(|i| i.id).collect();
    ids.sort_unstable();
    ids.dedup();
    let wipe = || async {
        for &id in &ids {
            s.delete_item(id).await;
        }
    };

    wipe().await;
    for step in steps {
        for item in step {
            s.put(item.clone()).await.unwrap();
        }
    }
    let singles = snap(s, &ids).await;
    let singles_hits = search_ids(s, "zebra").await;

    wipe().await;
    for step in steps {
        s.put_batch(step).await.unwrap();
    }
    let batched = snap(s, &ids).await;
    let batched_hits = search_ids(s, "zebra").await;

    assert_eq!(batched, singles, "{what}: rows differ from single puts");
    assert_eq!(
        batched_hits, singles_hits,
        "{what}: FTS hits differ from single puts"
    );
}

async fn run<S: Inspect>(s: &S) {
    // Every column family, including the language index triggers.
    let items: Vec<_> = (0..9).map(|n| rich(BASE + n as u64, n)).collect();
    assert_batch_equals_singles(s, std::slice::from_ref(&items), "mixed kinds").await;
    let snaps = snap(s, &items.iter().map(|i| i.id).collect::<Vec<_>>()).await;
    assert!(
        snaps.iter().any(|x| !x.langs.is_empty()),
        "fixture must exercise the language index"
    );
    assert!(
        snaps[0].title_fold.contains("édition"),
        "title_fold is the unicode-folded title"
    );

    // 0 and 1 items.
    s.put_batch(&[]).await.unwrap();
    assert_batch_equals_singles(s, &[vec![rich(BASE + 100, 1)]], "one item").await;

    // Later occurrence of an id wins, in the same batch.
    let mut first = rich(BASE + 200, 0);
    first.title = "Zebra first".into();
    first.probe.audio_tracks = vec![track("deu")];
    let mut last = rich(BASE + 200, 0);
    last.title = "Zebra last".into();
    assert_batch_equals_singles(
        s,
        &[vec![first, rich(BASE + 201, 3), last]],
        "duplicate id in one batch",
    )
    .await;
    assert_eq!(s.get(BASE + 200).await.unwrap().title, "Zebra last");
    if let Some(txs) = s.writer_transactions(BASE + 200, BASE + 202).await {
        assert_eq!(txs, 1, "a repeated id is collapsed, not retried per item");
    }

    // Upsert over an existing row: created_at is kept, the language index is
    // rebuilt from the new tracks.
    let mut v1 = rich(BASE + 300, 0);
    v1.created_at = Some(100);
    let mut v2 = rich(BASE + 300, 0);
    v2.created_at = Some(999);
    v2.title = "Zebra revised".into();
    v2.probe.audio_tracks = vec![track("kor")];
    assert_batch_equals_singles(s, &[vec![v1], vec![v2]], "upsert over existing").await;
    assert_eq!(s.get(BASE + 300).await.unwrap().created_at, Some(100));

    // More rows than one statement can carry (SQLite 500, Postgres 1000), with
    // a duplicate that straddles the statement boundary.
    let big: Vec<_> = (0..2300)
        .map(|n| rich(BASE + 10_000 + n as u64, n))
        .collect();
    let mut with_dup = big.clone();
    let mut middle = rich(BASE + 10_000, 0);
    middle.title = "Zebra middle".into();
    with_dup.insert(3, middle);
    let mut again = rich(BASE + 10_000, 0);
    again.title = "Zebra again".into();
    with_dup.push(again);
    s.put_batch(&with_dup).await.unwrap();
    assert_eq!(
        s.count_between(BASE + 10_000, BASE + 12_300).await,
        2300,
        "every row of a multi-statement batch is written exactly once"
    );
    assert_eq!(s.get(BASE + 10_000).await.unwrap().title, "Zebra again");
    if let Some(txs) = s.writer_transactions(BASE + 10_000, BASE + 12_300).await {
        assert_eq!(txs, 1, "the batch is one transaction, not a per-item retry");
    }
    for n in [1_usize, 499, 500, 1_000, 1_001, 2_299] {
        let id = BASE + 10_000 + n as u64;
        assert_eq!(s.get(id).await.unwrap(), big[n], "row {n}");
    }
    for n in 0..2300_u64 {
        s.delete_item(BASE + 10_000 + n).await;
    }

    // A rejected item: the transaction rolls back, the rest are written one by
    // one, the error comes back. Two ways to be rejected, both from the data.
    let good1 = rich(BASE + 400, 0);
    let mut path_clash = rich(BASE + 401, 0);
    path_clash.path = good1.path.clone();
    let good2 = rich(BASE + 402, 0);
    let mut bad_path = rich(BASE + 403, 0);
    bad_path.path = non_utf8_path();
    for id in 400..404 {
        s.delete_item(BASE + id).await;
    }
    let err = s
        .put_batch(&[good1, path_clash, good2, bad_path])
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Backend(_)), "got {err:?}");
    assert_eq!(s.get(BASE + 400).await.unwrap().id, BASE + 400);
    assert_eq!(s.get(BASE + 402).await.unwrap().id, BASE + 402);
    assert!(s.get(BASE + 401).await.is_err(), "clashing path not stored");
    assert!(s.get(BASE + 403).await.is_err(), "non-utf8 path not stored");
    assert_eq!(
        s.count_between(BASE + 400, BASE + 403).await,
        2,
        "exactly the two good rows"
    );
}

fn non_utf8_path() -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(vec![b'/', b'p', 0xff, b'.', b'm', b'k', b'v']).into()
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use pharos_store_sqlx::sqlite::SqliteStore;

    impl Inspect for SqliteStore {
        async fn title_fold_sort_name(&self, id: MediaId) -> (String, String) {
            sqlx::query_as("SELECT title_fold, sort_name FROM media_items WHERE id = ?")
                .bind(id as i64)
                .fetch_one(self.pool())
                .await
                .unwrap()
        }

        async fn language_rows(&self, id: MediaId) -> Vec<(String, String)> {
            sqlx::query_as(
                "SELECT kind, lang FROM media_item_languages WHERE item_id = ? ORDER BY kind, lang",
            )
            .bind(id as i64)
            .fetch_all(self.pool())
            .await
            .unwrap()
        }

        async fn delete_item(&self, id: MediaId) {
            sqlx::query("DELETE FROM media_items WHERE id = ?")
                .bind(id as i64)
                .execute(self.pool())
                .await
                .unwrap();
        }

        async fn count_between(&self, lo: MediaId, hi: MediaId) -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM media_items WHERE id >= ? AND id < ?")
                .bind(lo as i64)
                .bind(hi as i64)
                .fetch_one(self.pool())
                .await
                .unwrap()
        }

        async fn writer_transactions(&self, _lo: MediaId, _hi: MediaId) -> Option<i64> {
            None
        }
    }

    #[tokio::test]
    async fn sqlite_put_batch_matches_single_puts() {
        let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
        run(&s).await;
    }
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use pharos_store_sqlx::postgres::PostgresStore;

    impl Inspect for PostgresStore {
        async fn title_fold_sort_name(&self, id: MediaId) -> (String, String) {
            sqlx::query_as("SELECT title_fold, sort_name FROM media_items WHERE id = $1")
                .bind(id as i64)
                .fetch_one(self.pool())
                .await
                .unwrap()
        }

        async fn language_rows(&self, id: MediaId) -> Vec<(String, String)> {
            sqlx::query_as(
                "SELECT kind, lang FROM media_item_languages WHERE item_id = $1 ORDER BY kind, lang",
            )
            .bind(id as i64)
            .fetch_all(self.pool())
            .await
            .unwrap()
        }

        async fn delete_item(&self, id: MediaId) {
            sqlx::query("DELETE FROM media_items WHERE id = $1")
                .bind(id as i64)
                .execute(self.pool())
                .await
                .unwrap();
        }

        async fn count_between(&self, lo: MediaId, hi: MediaId) -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM media_items WHERE id >= $1 AND id < $2")
                .bind(lo as i64)
                .bind(hi as i64)
                .fetch_one(self.pool())
                .await
                .unwrap()
        }

        async fn writer_transactions(&self, lo: MediaId, hi: MediaId) -> Option<i64> {
            // xmin is the inserting/updating transaction of the row version.
            // The rows were deleted first, so every version here is new.
            Some(
                sqlx::query_scalar(
                    "SELECT COUNT(DISTINCT xmin::text) FROM media_items WHERE id >= $1 AND id < $2",
                )
                .bind(lo as i64)
                .bind(hi as i64)
                .fetch_one(self.pool())
                .await
                .unwrap(),
            )
        }
    }

    #[tokio::test]
    async fn postgres_put_batch_matches_single_puts() {
        let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
            eprintln!("SKIP postgres_put_batch: PHAROS_TEST_POSTGRES_URL unset");
            return;
        };
        let s = PostgresStore::connect(&url).await.unwrap();
        run(&s).await;
    }
}
