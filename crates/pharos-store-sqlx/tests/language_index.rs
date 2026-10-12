//! Migration 0056 (`media_item_languages`): the rows are derived from the track
//! blobs by database triggers, so they follow `put()`, the scan sweep and any
//! other writer, and a database that predates the migration is indexed by the
//! migration itself. Postgres runs the same scenarios whenever
//! `PHAROS_TEST_POSTGRES_URL` is set (`just test-postgres`).

#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    AudioTrack, MediaFilters, MediaItem, MediaKind, MediaProbe, MediaQuery, MediaStore,
    SubtitleTrack,
};
use pharos_store_sqlx::sqlite::SqliteStore;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::borrow::Cow;
use std::str::FromStr;

/// The last migration before the language index was added.
const BEFORE_LANGUAGE_INDEX: i64 = 55;

fn audio(lang: &str) -> AudioTrack {
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

fn movie(id: u64, audio_tracks: Vec<AudioTrack>, subtitle_tracks: Vec<SubtitleTrack>) -> MediaItem {
    MediaItem {
        id,
        path: format!("/a/{id}.mkv").into(),
        title: format!("Movie {id}"),
        kind: MediaKind::Movie,
        probe: MediaProbe {
            audio_tracks,
            subtitle_tracks,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn row(kind: &str, lang: &str) -> (String, String) {
    (kind.to_string(), lang.to_string())
}

/// What the tests read back from, and write behind the back of, either backend.
trait Inspect {
    async fn language_rows(&self, id: i64) -> Vec<(String, String)>;
    /// Rewrite the audio blob the way a binary that predates the index would:
    /// a plain UPDATE, no index rows.
    async fn write_audio_blob(&self, id: i64, blob: &str);
    /// Delete the item the way any writer could, without going through
    /// `sweep_unseen`.
    async fn delete_item(&self, id: i64);
    /// Rewrite one index row.
    async fn retag_row(&self, id: i64, from: &str, to: &str);
    /// Start counting every row write (insert, update, delete) to the index.
    async fn count_index_writes(&self);
    async fn index_writes(&self) -> i64;
    /// `(code, canon)` of the `language_codes` table.
    async fn language_codes(&self) -> Vec<(String, String)>;
}

impl Inspect for SqliteStore {
    async fn language_rows(&self, id: i64) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT kind, lang FROM media_item_languages WHERE item_id = ? ORDER BY kind, lang",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await
        .unwrap()
    }

    async fn write_audio_blob(&self, id: i64, blob: &str) {
        sqlx::query("UPDATE media_items SET audio_tracks_json = ? WHERE id = ?")
            .bind(blob)
            .bind(id)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn delete_item(&self, id: i64) {
        sqlx::query("DELETE FROM media_items WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn retag_row(&self, id: i64, from: &str, to: &str) {
        sqlx::query("UPDATE media_item_languages SET lang = ? WHERE item_id = ? AND lang = ?")
            .bind(to)
            .bind(id)
            .bind(from)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn count_index_writes(&self) {
        sqlx::raw_sql(
            "DROP TABLE IF EXISTS lang_writes; \
             DROP TRIGGER IF EXISTS lang_writes_i; \
             DROP TRIGGER IF EXISTS lang_writes_u; \
             DROP TRIGGER IF EXISTS lang_writes_d; \
             CREATE TABLE lang_writes (n INTEGER); \
             CREATE TRIGGER lang_writes_i AFTER INSERT ON media_item_languages \
               BEGIN INSERT INTO lang_writes VALUES (1); END; \
             CREATE TRIGGER lang_writes_u AFTER UPDATE ON media_item_languages \
               BEGIN INSERT INTO lang_writes VALUES (1); END; \
             CREATE TRIGGER lang_writes_d AFTER DELETE ON media_item_languages \
               BEGIN INSERT INTO lang_writes VALUES (1); END;",
        )
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn index_writes(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM lang_writes")
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    async fn language_codes(&self) -> Vec<(String, String)> {
        sqlx::query_as("SELECT code, canon FROM language_codes ORDER BY code")
            .fetch_all(self.pool())
            .await
            .unwrap()
    }
}

#[cfg(feature = "postgres")]
impl Inspect for pharos_store_sqlx::postgres::PostgresStore {
    async fn language_rows(&self, id: i64) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT kind, lang FROM media_item_languages WHERE item_id = $1 ORDER BY kind, lang",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await
        .unwrap()
    }

    async fn write_audio_blob(&self, id: i64, blob: &str) {
        sqlx::query("UPDATE media_items SET audio_tracks_json = $1 WHERE id = $2")
            .bind(blob)
            .bind(id)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn delete_item(&self, id: i64) {
        sqlx::query("DELETE FROM media_items WHERE id = $1")
            .bind(id)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn retag_row(&self, id: i64, from: &str, to: &str) {
        sqlx::query("UPDATE media_item_languages SET lang = $1 WHERE item_id = $2 AND lang = $3")
            .bind(to)
            .bind(id)
            .bind(from)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn count_index_writes(&self) {
        sqlx::raw_sql(
            "DROP TRIGGER IF EXISTS lang_writes_t ON media_item_languages; \
             DROP TABLE IF EXISTS lang_writes; \
             CREATE TABLE lang_writes (n INTEGER); \
             CREATE OR REPLACE FUNCTION lang_writes_fn() RETURNS trigger AS $$ \
               BEGIN INSERT INTO lang_writes VALUES (1); RETURN NULL; END $$ LANGUAGE plpgsql; \
             CREATE TRIGGER lang_writes_t AFTER INSERT OR UPDATE OR DELETE \
               ON media_item_languages FOR EACH ROW EXECUTE FUNCTION lang_writes_fn();",
        )
        .execute(self.pool())
        .await
        .unwrap();
    }

    async fn index_writes(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM lang_writes")
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    async fn language_codes(&self) -> Vec<(String, String)> {
        sqlx::query_as("SELECT code, canon FROM language_codes ORDER BY code")
            .fetch_all(self.pool())
            .await
            .unwrap()
    }
}

/// Raw access for the Postgres arms: the store on a dedicated, freshly created
/// database. Other test binaries share `PHAROS_TEST_POSTGRES_URL`'s database
/// and the cleanest isolation is one of our own.
#[cfg(feature = "postgres")]
async fn fresh_pg_url(shared_url: &str, db: &str) -> String {
    use sqlx::Connection;
    let (base, _shared_db) = shared_url
        .rsplit_once('/')
        .expect("postgres url has a db path");
    let mut admin = sqlx::postgres::PgConnection::connect(&format!("{base}/postgres"))
        .await
        .expect("connect postgres maintenance db");
    // DROP/CREATE DATABASE cannot run inside a transaction.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {db}")))
        .execute(&mut admin)
        .await
        .expect("drop stale test db");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&mut admin)
        .await
        .expect("create test db");
    format!("{base}/{db}")
}

#[cfg(feature = "postgres")]
async fn fresh_pg(shared_url: &str, db: &str) -> pharos_store_sqlx::postgres::PostgresStore {
    let url = fresh_pg_url(shared_url, db).await;
    pharos_store_sqlx::postgres::PostgresStore::connect_without_backfill(&url)
        .await
        .unwrap()
}

#[cfg(feature = "postgres")]
fn pg_url() -> Option<String> {
    std::env::var("PHAROS_TEST_POSTGRES_URL").ok()
}

/// put() leaves one canonical row per language, a re-put changes them, and the
/// scan sweep takes an item's rows with it. `base` keeps ids and paths clear
/// of other tests.
async fn put_and_sweep_keep_rows_in_step<S: MediaStore + Inspect>(s: &S, base: u64) {
    let (keep, gone) = (base, base + 1);
    let root = format!("/lang-index-{base}");
    let at = |id: u64, mut item: MediaItem| {
        item.path = format!("{root}/{id}.mkv").into();
        item
    };

    // fre / FRA / fr are one language; the two kinds stay separate.
    s.put(at(
        keep,
        movie(
            keep,
            vec![audio("fre"), audio("FRA"), audio("fr"), audio("eng")],
            vec![sub("ger"), sub("deu")],
        ),
    ))
    .await
    .unwrap();
    assert_eq!(
        s.language_rows(keep as i64).await,
        [
            row("audio", "eng"),
            row("audio", "fre"),
            row("subtitle", "ger")
        ]
    );

    // Changing the tracks re-derives the rows; nothing stale is left.
    s.put(at(keep, movie(keep, vec![audio("jpn")], vec![])))
        .await
        .unwrap();
    assert_eq!(s.language_rows(keep as i64).await, [row("audio", "jpn")]);

    // No tracks at all: no rows.
    s.put(at(gone, movie(gone, vec![audio("eng")], vec![sub("eng")])))
        .await
        .unwrap();
    s.put(at(gone, movie(gone, vec![], vec![]))).await.unwrap();
    assert!(s.language_rows(gone as i64).await.is_empty());

    // Only `keep` is seen by the scan; `gone` is swept with its rows.
    s.put(at(gone, movie(gone, vec![audio("eng")], vec![sub("eng")])))
        .await
        .unwrap();
    let scan = s.begin_scan(std::path::Path::new(&root)).await.unwrap();
    s.mark_seen(keep, scan, 1, 1).await.unwrap();
    assert_eq!(s.sweep_unseen(scan, &root).await.unwrap(), [gone]);
    assert!(s.language_rows(gone as i64).await.is_empty());
    assert_eq!(s.language_rows(keep as i64).await, [row("audio", "jpn")]);
}

/// Ids (relative to `base`) of the items under `root` passing the language
/// filters.
async fn matching<S: MediaStore>(
    s: &S,
    root: &str,
    base: u64,
    audio: &[&str],
    subs: &[&str],
) -> Vec<u64> {
    let q = MediaQuery {
        filters: MediaFilters {
            path_prefix: Some(root.to_string()),
            audio_languages: audio.iter().map(|l| l.to_string()).collect(),
            subtitle_languages: subs.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        },
        ..Default::default()
    };
    let (items, total) = s.query(&q).await.unwrap();
    assert_eq!(total as usize, items.len());
    let mut ids: Vec<u64> = items.iter().map(|i| i.id - base).collect();
    ids.sort_unstable();
    ids
}

/// A: audio fre, subs eng.  B: audio fra, subs deu.  C: audio eng, subs fre.
/// D: audio fr.  E: audio jpn, subs eng.
async fn seed_five<S: MediaStore>(s: &S, base: u64, root: &str) {
    let rows = [
        (0, vec![audio("fre")], vec![sub("eng")]),
        (1, vec![audio("fra")], vec![sub("deu")]),
        (2, vec![audio("eng")], vec![sub("fre")]),
        (3, vec![audio("fr")], vec![]),
        (4, vec![audio("jpn")], vec![sub("eng")]),
    ];
    for (n, a, t) in rows {
        let mut item = movie(base + n, a, t);
        item.path = format!("{root}/{n}.mkv").into();
        s.put(item).await.unwrap();
    }
}

/// OR within a list, AND across the two, fre/fra/fr one language, spellings
/// of the request folded, blank lists inert, and a re-put moves the item.
async fn filters_are_correct<S: MediaStore>(s: &S, base: u64) {
    let root = format!("/lang-filter-{base}");
    seed_five(s, base, &root).await;

    macro_rules! m {
        ($a:expr, $t:expr) => {
            matching(s, &root, base, $a, $t)
        };
    }
    assert_eq!(m!(&["fre"], &[]).await, [0, 1, 3], "fre");
    assert_eq!(m!(&["fra"], &[]).await, [0, 1, 3], "fra");
    assert_eq!(m!(&["FR"], &[]).await, [0, 1, 3], "fr");
    assert_eq!(m!(&["eng"], &[]).await, [2], "eng");
    assert_eq!(m!(&["fre", "jpn"], &[]).await, [0, 1, 3, 4], "or");
    assert_eq!(m!(&[], &["eng"]).await, [0, 4], "subs eng");
    assert_eq!(m!(&[], &["fra"]).await, [2], "subs fra");
    assert_eq!(m!(&["fre"], &["eng"]).await, [0], "and");
    assert_eq!(m!(&["fre"], &["deu"]).await, [1], "and, T spelling");
    assert_eq!(m!(&["fre"], &["jpn"]).await, Vec::<u64>::new(), "and, none");
    assert_eq!(m!(&["zzz"], &[]).await, Vec::<u64>::new(), "unknown");
    assert_eq!(m!(&[" "], &[]).await, [0, 1, 2, 3, 4], "blank is inert");

    let mut changed = movie(base, vec![audio("eng")], vec![]);
    changed.path = format!("{root}/0.mkv").into();
    s.put(changed).await.unwrap();
    assert_eq!(m!(&["fre"], &[]).await, [1, 3]);
    assert_eq!(m!(&["eng"], &[]).await, [0, 2]);
    assert_eq!(m!(&[], &["eng"]).await, [4]);
}

/// The filter reads the index, not the blob: with a row retagged the blob
/// still says `fra` yet the filter follows the table.
async fn filter_reads_the_table<S: MediaStore + Inspect>(s: &S, base: u64) {
    let root = format!("/lang-table-{base}");
    let mut item = movie(base, vec![audio("fra")], vec![]);
    item.path = format!("{root}/0.mkv").into();
    s.put(item).await.unwrap();
    assert_eq!(matching(s, &root, base, &["fre"], &[]).await, [0]);

    s.retag_row(base as i64, "fre", "ger").await;
    assert_eq!(
        matching(s, &root, base, &["fre"], &[]).await,
        Vec::<u64>::new()
    );
    assert_eq!(matching(s, &root, base, &["ger"], &[]).await, [0]);
}

/// A writer that does not go through `put()` (a replica on an older binary
/// during a rolling deploy, a hand-run statement) keeps the rows right too:
/// the triggers derive them from the blobs.
async fn any_writer_keeps_the_rows_right<S: MediaStore + Inspect>(s: &S, base: u64) {
    let root = format!("/lang-trigger-{base}");
    let id = base as i64;
    let mut item = movie(base, vec![audio("eng")], vec![]);
    item.path = format!("{root}/0.mkv").into();
    s.put(item).await.unwrap();

    let french = pharos_store_sqlx::audio_track_json::encode(&[audio("fra")]).unwrap();
    s.write_audio_blob(id, &french).await;
    assert_eq!(s.language_rows(id).await, [row("audio", "fre")]);
    assert_eq!(matching(s, &root, base, &["fre"], &[]).await, [0]);
    assert_eq!(
        matching(s, &root, base, &["eng"], &[]).await,
        Vec::<u64>::new()
    );

    // A blob that is not a track list is "no tracks", not an error.
    s.write_audio_blob(id, "this is not json").await;
    assert!(s.language_rows(id).await.is_empty());
    s.write_audio_blob(id, r#"{"language":"eng"}"#).await;
    assert!(s.language_rows(id).await.is_empty());
    // An object whose members are track-shaped is still not a track list.
    s.write_audio_blob(id, r#"{"a":{"language":"eng"}}"#).await;
    assert!(s.language_rows(id).await.is_empty());
    s.write_audio_blob(
        id,
        r#"["eng", 3, {"language": " EN "}, {"language": ""}, {}]"#,
    )
    .await;
    assert_eq!(s.language_rows(id).await, [row("audio", "eng")]);

    // Deleting the item takes its rows, whichever writer deletes it.
    s.delete_item(id).await;
    assert!(s.language_rows(id).await.is_empty());
}

/// A rescan re-puts every item. When its tracks are unchanged that must not
/// touch the index; when they change the item's rows are rebuilt.
async fn rescans_of_unchanged_items_write_no_index_rows<S: MediaStore + Inspect>(s: &S, base: u64) {
    s.count_index_writes().await;
    let id = base as i64;
    let put = |a: Vec<AudioTrack>, t: Vec<SubtitleTrack>| async {
        s.put(movie(base, a, t)).await.unwrap();
    };

    put(vec![audio("eng"), audio("fre")], vec![sub("ger")]).await;
    assert_eq!(s.index_writes().await, 3, "first put writes its rows");

    put(vec![audio("eng"), audio("fre")], vec![sub("ger")]).await;
    assert_eq!(s.index_writes().await, 3, "rescan of an unchanged item");

    // A changed blob (track order) rebuilds the rows: three deletes, three
    // inserts.
    put(vec![audio("fra"), audio("eng")], vec![sub("deu")]).await;
    assert_eq!(s.index_writes().await, 9, "changed blob rebuilds the rows");

    put(vec![audio("fra"), audio("eng")], vec![sub("deu")]).await;
    assert_eq!(s.index_writes().await, 9, "and a rescan is quiet again");

    put(vec![audio("fra"), audio("jpn")], vec![sub("deu")]).await;
    assert_eq!(
        s.language_rows(id).await,
        [
            row("audio", "fre"),
            row("audio", "jpn"),
            row("subtitle", "ger")
        ]
    );
}

/// The scanner stamps every item it sees with mark_seen / mark_seen_batch,
/// which UPDATE `media_items` without touching the track blobs. That must not
/// touch the index.
async fn stamping_items_seen_writes_no_index_rows<S: MediaStore + Inspect>(s: &S, base: u64) {
    let root = format!("/lang-seen-{base}");
    let mut item = movie(base, vec![audio("eng"), audio("fre")], vec![sub("ger")]);
    item.path = format!("{root}/0.mkv").into();
    s.put(item).await.unwrap();
    s.count_index_writes().await;

    let scan = s.begin_scan(std::path::Path::new(&root)).await.unwrap();
    s.mark_seen(base, scan, 10, 20).await.unwrap();
    s.mark_seen_batch(&[(base, 11, 21)], scan).await.unwrap();
    assert_eq!(s.index_writes().await, 0, "stamping an item seen");
    assert_eq!(s.language_rows(base as i64).await.len(), 3);
}

/// `language_codes` is the SQL copy of `pharos_core::language`'s table: the
/// same spellings, each mapped to the form `canonical_language` gives.
async fn language_codes_match_the_rust_table<S: Inspect>(s: &S) {
    let sql = s.language_codes().await;
    let mut rust: Vec<(String, String)> = pharos_core::language::known_codes()
        .map(|c| (c.to_string(), pharos_core::language::canonical_language(c)))
        .collect();
    rust.sort();
    rust.dedup();
    assert_eq!(sql, rust);
}

#[tokio::test]
async fn sqlite_put_and_sweep_keep_rows_in_step() {
    let s = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    put_and_sweep_keep_rows_in_step(&s, 1).await;
}

#[tokio::test]
async fn sqlite_filters_are_correct() {
    let s = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    filters_are_correct(&s, 100).await;
}

#[tokio::test]
async fn sqlite_filter_reads_the_table() {
    let s = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    filter_reads_the_table(&s, 200).await;
}

#[tokio::test]
async fn sqlite_any_writer_keeps_the_rows_right() {
    let s = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    any_writer_keeps_the_rows_right(&s, 400).await;
}

#[tokio::test]
async fn sqlite_rescans_of_unchanged_items_write_no_index_rows() {
    let s = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    rescans_of_unchanged_items_write_no_index_rows(&s, 500).await;
}

#[tokio::test]
async fn sqlite_stamping_items_seen_writes_no_index_rows() {
    let s = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    stamping_items_seen_writes_no_index_rows(&s, 600).await;
}

#[tokio::test]
async fn sqlite_language_codes_match_the_rust_table() {
    let s = SqliteStore::connect_without_backfill("sqlite::memory:")
        .await
        .unwrap();
    language_codes_match_the_rust_table(&s).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_language_index() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP postgres_language_index: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    let s = fresh_pg(&url, "pharos_langidx").await;
    put_and_sweep_keep_rows_in_step(&s, 920_056).await;
    filters_are_correct(&s, 940_100).await;
    filter_reads_the_table(&s, 940_200).await;
    any_writer_keeps_the_rows_right(&s, 940_400).await;
    rescans_of_unchanged_items_write_no_index_rows(&s, 940_500).await;
    stamping_items_seen_writes_no_index_rows(&s, 940_600).await;
    language_codes_match_the_rust_table(&s).await;
}

fn scratch_db_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pharos-language-index-migration-{}-{nanos}.db",
        std::process::id()
    ))
}

/// Track blobs for the rows a database that predates the migration holds:
/// `(id, audio blob, subtitle blob, expected rows)`.
#[allow(clippy::type_complexity)]
fn old_rows() -> Vec<(i64, Option<String>, Option<String>, Vec<(String, String)>)> {
    let fre = pharos_store_sqlx::audio_track_json::encode(&[audio("fre"), audio("FRA")]);
    let eng_sub = pharos_store_sqlx::subtitle_track_json::encode(&[sub("eng")]);
    let both = vec![row("audio", "fre"), row("subtitle", "eng")];
    vec![
        (0, fre.clone(), eng_sub.clone(), both.clone()),
        (1, fre, eng_sub, both),
        (2, None, None, vec![]),
        // Not JSON, not an array, and an array of odd elements: the Rust
        // decoder reads the first two as "no tracks"; none may fail the
        // migration.
        (3, Some("not json".into()), None, vec![]),
        (4, Some(r#"{"a":{"language":"eng"}}"#.into()), None, vec![]),
        (
            5,
            Some(
                r#"["eng", 3, {"language": " EN "}, {"language": ""}, {"language": "jpn"}]"#.into(),
            ),
            Some(r#"[{"language":"deu"}]"#.into()),
            vec![
                row("audio", "eng"),
                row("audio", "jpn"),
                row("subtitle", "ger"),
            ],
        ),
    ]
}

/// A database at schema 0055 holding `old_rows()` plus `filler` blob-less
/// movies.
async fn populated_old_db(url: &str, filler: usize) {
    let opts = SqliteConnectOptions::from_str(url)
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let mut migrator = sqlx::migrate!("./migrations/sqlite");
    migrator.migrations = Cow::Owned(
        migrator
            .migrations
            .iter()
            .filter(|m| m.version <= BEFORE_LANGUAGE_INDEX)
            .cloned()
            .collect(),
    );
    migrator.run(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let rows = old_rows();
    for i in 0..(rows.len() + filler) {
        let (a, s) = rows
            .get(i)
            .map_or((None, None), |(_, a, s, _)| (a.clone(), s.clone()));
        sqlx::query(
            "INSERT INTO media_items (id, path, title, kind, audio_tracks_json, \
             subtitle_tracks_json) VALUES (?, ?, ?, 'movie', ?, ?)",
        )
        .bind(i64::try_from(i).unwrap())
        .bind(format!("/m/{i}.mkv"))
        .bind(format!("Movie {i}"))
        .bind(a)
        .bind(s)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    pool.close().await;
}

/// The migration indexes the rows a populated database already holds, with
/// the spellings folded, and tolerates blobs it cannot read.
#[tokio::test]
async fn sqlite_migration_indexes_existing_rows() {
    let path = scratch_db_path();
    let url = format!("sqlite://{}", path.display());
    populated_old_db(&url, 600).await;

    let store = SqliteStore::connect_without_backfill(&url).await.unwrap();
    for (id, _, _, expected) in old_rows() {
        assert_eq!(store.language_rows(id).await, expected, "item {id}");
    }
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_item_languages")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(total, 2 + 2 + 3);
    // Reopening applies nothing again and leaves the rows alone.
    store.pool().close().await;
    let again = SqliteStore::connect_without_backfill(&url).await.unwrap();
    let (after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_item_languages")
        .fetch_one(again.pool())
        .await
        .unwrap();
    assert_eq!(after, total);
    again.pool().close().await;
    let _ = std::fs::remove_file(&path);
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_migration_indexes_existing_rows() {
    use pharos_store_sqlx::postgres::PostgresStore;
    use sqlx::postgres::PgPoolOptions;
    let Some(shared) = pg_url() else {
        eprintln!("SKIP postgres_migration_indexes_existing_rows: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    let url = fresh_pg_url(&shared, "pharos_langidx_migrate").await;

    // Schema as it was before the migration, with rows in it.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let mut migrator = sqlx::migrate!("./migrations/postgres");
    migrator.migrations = Cow::Owned(
        migrator
            .migrations
            .iter()
            .filter(|m| m.version <= BEFORE_LANGUAGE_INDEX)
            .cloned()
            .collect(),
    );
    migrator.run(&pool).await.unwrap();
    for (id, a, s, _) in old_rows() {
        sqlx::query(
            "INSERT INTO media_items (id, path, title, kind, audio_tracks_json, \
             subtitle_tracks_json) VALUES ($1, $2, 'Movie', 'movie', $3, $4)",
        )
        .bind(id)
        .bind(format!("/m/{id}.mkv"))
        .bind(a)
        .bind(s)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;

    // Connecting applies the language migration over them.
    let store = PostgresStore::connect_without_backfill(&url).await.unwrap();
    for (id, _, _, expected) in old_rows() {
        assert_eq!(store.language_rows(id).await, expected, "item {id}");
    }
}
