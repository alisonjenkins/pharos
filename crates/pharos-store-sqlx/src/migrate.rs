//! One-shot SQLite -> Postgres data migration (`pharos admin db-migrate`).
//!
//! Copies every domain table from a live `SqliteStore` into a freshly
//! migrated (empty) `PostgresStore`, preserving primary-key ids so
//! association tables (item_genres, playlist_items, …) keep resolving.
//! Runs as a single postgres transaction: any failure rolls back cleanly,
//! leaving the target database untouched. After the copy, `setval` bumps
//! every identity/serial sequence past the copied max id so future inserts
//! don't collide, and a final count-verification pass (run after commit,
//! independently on both pools) confirms every row made it across.
//!
//! Table order is FK-safe: parents first (users, libraries, genres,
//! people, studios, tags, collections, playlists, media_items,
//! system_identity, runtime_config, named_config, scan_runs), then the
//! dependents that reference them (auth_tokens, user_data,
//! user_configuration, display_preferences, artwork, item_genres,
//! item_people, item_studios, item_tags, collection_items,
//! playlist_items).
//!
//! Type notes (see the individual copy functions): there are no BOOLEAN
//! columns in either backend (booleans are 0/1 INTEGER), and postgres
//! implicitly assignment-casts a wider bind (i64 into an INTEGER column,
//! f64 into a REAL column) when it lands in an INSERT target column, so
//! every integer field is read from sqlite and bound into postgres as
//! `i64` uniformly regardless of the declared column width, and every
//! REAL/real field as `f64`. BLOB / BYTEA columns round-trip as `Vec<u8>`.

// Every copy function reads a row shape straight off its SQL SELECT into a
// tuple (or, for media_items, a dedicated FromRow struct) — wide tuples are
// the simplest honest representation of a wide table's columns here, so
// the type-complexity lint is silenced file-wide rather than aliased away
// table-by-table.
#![allow(clippy::type_complexity)]

use crate::postgres::PostgresStore;
use crate::sqlite::SqliteStore;
use crate::StoreError;
use sqlx::{PgPool, Postgres, SqlitePool, Transaction};

/// Per-table row counts copied, in copy order.
pub struct MigrationReport {
    pub tables: Vec<(String, u64)>,
}

/// Copy every domain table from `src` (SQLite) into `dst` (Postgres). `dst`
/// must be an empty database — the postgres schema migrations already ran
/// at `PostgresStore::connect` time, so the tables exist but hold no rows.
/// Verifies row counts match on both sides before returning; on any
/// mismatch (or sqlx error) the target's insert transaction has already
/// rolled back, so a failed call leaves `dst` untouched.
pub async fn migrate_sqlite_to_postgres(
    src: &SqliteStore,
    dst: &PostgresStore,
) -> Result<MigrationReport, StoreError> {
    let sp = src.pool();
    let dp = dst.pool();
    let mut tx = dp.begin().await.map_err(StoreError::Sqlx)?;

    let mut tables: Vec<(String, u64)> = Vec::new();

    // -- Parents ---------------------------------------------------
    tables.push(("users".into(), copy_users(sp, &mut tx).await?));
    tables.push(("libraries".into(), copy_libraries(sp, &mut tx).await?));
    tables.push((
        "genres".into(),
        copy_id_name_wire(sp, &mut tx, "genres").await?,
    ));
    tables.push(("people".into(), copy_people(sp, &mut tx).await?));
    tables.push((
        "studios".into(),
        copy_id_name_wire(sp, &mut tx, "studios").await?,
    ));
    tables.push(("tags".into(), copy_id_name_wire(sp, &mut tx, "tags").await?));
    tables.push(("collections".into(), copy_collections(sp, &mut tx).await?));
    tables.push(("playlists".into(), copy_playlists(sp, &mut tx).await?));
    tables.push(("media_items".into(), copy_media_items(sp, &mut tx).await?));
    tables.push((
        "system_identity".into(),
        copy_system_identity(sp, &mut tx).await?,
    ));
    tables.push((
        "runtime_config".into(),
        copy_runtime_config(sp, &mut tx).await?,
    ));
    tables.push(("named_config".into(), copy_named_config(sp, &mut tx).await?));
    tables.push(("scan_runs".into(), copy_scan_runs(sp, &mut tx).await?));

    // -- Dependents --------------------------------------------------
    tables.push(("auth_tokens".into(), copy_auth_tokens(sp, &mut tx).await?));
    tables.push(("user_data".into(), copy_user_data(sp, &mut tx).await?));
    tables.push((
        "user_configuration".into(),
        copy_user_configuration(sp, &mut tx).await?,
    ));
    tables.push((
        "display_preferences".into(),
        copy_display_preferences(sp, &mut tx).await?,
    ));
    tables.push(("artwork".into(), copy_artwork(sp, &mut tx).await?));
    tables.push((
        "item_genres".into(),
        copy_item_link(sp, &mut tx, "item_genres", "genre_id").await?,
    ));
    tables.push(("item_people".into(), copy_item_people(sp, &mut tx).await?));
    tables.push((
        "item_studios".into(),
        copy_item_link(sp, &mut tx, "item_studios", "studio_id").await?,
    ));
    tables.push((
        "item_tags".into(),
        copy_item_link(sp, &mut tx, "item_tags", "tag_id").await?,
    ));
    tables.push((
        "collection_items".into(),
        copy_collection_items(sp, &mut tx).await?,
    ));
    tables.push((
        "playlist_items".into(),
        copy_playlist_items(sp, &mut tx).await?,
    ));

    tx.commit().await.map_err(StoreError::Sqlx)?;

    verify_counts(sp, dp, &tables).await?;

    Ok(MigrationReport { tables })
}

/// Bump `table`'s id sequence past the copied max id (identity + serial
/// columns alike — `pg_get_serial_sequence` resolves both), so the next
/// app-driven insert doesn't collide with a migrated id.
async fn setval(tx: &mut Transaction<'_, Postgres>, table: &str) -> Result<(), StoreError> {
    let sql = format!(
        "SELECT setval(pg_get_serial_sequence('{table}', 'id'), \
         COALESCE((SELECT MAX(id) FROM {table}), 1))"
    );
    sqlx::query(&sql)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    Ok(())
}

/// Independent post-commit check: every copied table's sqlite row count
/// must equal its postgres row count. Named per-table so a mismatch
/// pinpoints exactly which table under/over-copied.
async fn verify_counts(
    sp: &SqlitePool,
    dp: &PgPool,
    tables: &[(String, u64)],
) -> Result<(), StoreError> {
    for (table, _) in tables {
        let (sqlite_count,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(sp)
            .await
            .map_err(StoreError::Sqlx)?;
        let (pg_count,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(dp)
            .await
            .map_err(StoreError::Sqlx)?;
        if sqlite_count != pg_count {
            return Err(StoreError::Parse(format!(
                "migration count mismatch on {table}: sqlite={sqlite_count} postgres={pg_count}"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Parents
// ---------------------------------------------------------------------

async fn copy_users(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(Vec<u8>, String, String, i64)> =
        sqlx::query_as("SELECT id, name, password_hash, admin FROM users")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, name, password_hash, admin) in rows {
        sqlx::query("INSERT INTO users (id, name, password_hash, admin) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(name)
            .bind(password_hash)
            .bind(admin)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_libraries(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(
        i64,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
    )> = sqlx::query_as(
        "SELECT id, name, root_path, kind, wire_id, options, created_at FROM libraries",
    )
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, name, root_path, kind, wire_id, options, created_at) in rows {
        sqlx::query(
            "INSERT INTO libraries (id, name, root_path, kind, wire_id, options, created_at) \
             OVERRIDING SYSTEM VALUE VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(name)
        .bind(root_path)
        .bind(kind)
        .bind(wire_id)
        .bind(options)
        .bind(created_at)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    setval(tx, "libraries").await?;
    Ok(n)
}

/// Shared shape for the `(id, name, wire_id)` identity entity tables
/// (genres / studios / tags).
async fn copy_id_name_wire(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, String, String)> =
        sqlx::query_as(&format!("SELECT id, name, wire_id FROM {table}"))
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, name, wire_id) in rows {
        sqlx::query(&format!(
            "INSERT INTO {table} (id, name, wire_id) OVERRIDING SYSTEM VALUE VALUES ($1, $2, $3)"
        ))
        .bind(id)
        .bind(name)
        .bind(wire_id)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    setval(tx, table).await?;
    Ok(n)
}

async fn copy_people(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(
        i64,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as("SELECT id, name, sort_name, wire_id, provider_ids, thumb_url FROM people")
        .fetch_all(sp)
        .await
        .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, name, sort_name, wire_id, provider_ids, thumb_url) in rows {
        sqlx::query(
            "INSERT INTO people (id, name, sort_name, wire_id, provider_ids, thumb_url) \
             OVERRIDING SYSTEM VALUE VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(name)
        .bind(sort_name)
        .bind(wire_id)
        .bind(provider_ids)
        .bind(thumb_url)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    setval(tx, "people").await?;
    Ok(n)
}

async fn copy_collections(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, String, String, String, Option<String>)> =
        sqlx::query_as("SELECT id, name, wire_id, kind, overview FROM collections")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, name, wire_id, kind, overview) in rows {
        sqlx::query(
            "INSERT INTO collections (id, name, wire_id, kind, overview) \
             OVERRIDING SYSTEM VALUE VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(name)
        .bind(wire_id)
        .bind(kind)
        .bind(overview)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    setval(tx, "collections").await?;
    Ok(n)
}

async fn copy_playlists(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, String, String, Option<String>, String, i64)> = sqlx::query_as(
        "SELECT id, wire_id, name, owner_user_id, media_type, created_at FROM playlists",
    )
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, wire_id, name, owner_user_id, media_type, created_at) in rows {
        // playlists.id is a plain BIGSERIAL (GENERATED BY DEFAULT), not a
        // GENERATED ALWAYS identity column, so an explicit id inserts
        // without OVERRIDING SYSTEM VALUE.
        sqlx::query(
            "INSERT INTO playlists (id, wire_id, name, owner_user_id, media_type, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(wire_id)
        .bind(name)
        .bind(owner_user_id)
        .bind(media_type)
        .bind(created_at)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    setval(tx, "playlists").await?;
    Ok(n)
}

async fn copy_system_identity(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT id, server_id, created_at FROM system_identity")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, server_id, created_at) in rows {
        sqlx::query("INSERT INTO system_identity (id, server_id, created_at) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(server_id)
            .bind(created_at)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_runtime_config(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, Option<String>, Option<String>, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, server_name, login_disclaimer, custom_css, updated_at FROM runtime_config",
    )
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, server_name, login_disclaimer, custom_css, updated_at) in rows {
        sqlx::query(
            "INSERT INTO runtime_config \
             (id, server_name, login_disclaimer, custom_css, updated_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(server_name)
        .bind(login_disclaimer)
        .bind(custom_css)
        .bind(updated_at)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_named_config(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT key, value, updated_at FROM named_config")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (key, value, updated_at) in rows {
        sqlx::query("INSERT INTO named_config (key, value, updated_at) VALUES ($1, $2, $3)")
            .bind(key)
            .bind(value)
            .bind(updated_at)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_scan_runs(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(
        i64,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    )> = sqlx::query_as(
        "SELECT id, root, started_at, finished_at, items_seen, items_swept FROM scan_runs",
    )
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (id, root, started_at, finished_at, items_seen, items_swept) in rows {
        sqlx::query(
            "INSERT INTO scan_runs (id, root, started_at, finished_at, items_seen, items_swept) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(root)
        .bind(started_at)
        .bind(finished_at)
        .bind(items_seen)
        .bind(items_swept)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    setval(tx, "scan_runs").await?;
    Ok(n)
}

// ---------------------------------------------------------------------
// media_items — the widest table; every column added since migration
// 0001, in the order the postgres migrations introduced them. NOTE:
// `search_tsv` (0029) is `GENERATED ALWAYS AS (...) STORED` and is
// deliberately excluded — postgres computes it, inserting into it is an
// error.
// ---------------------------------------------------------------------

const MEDIA_MIGRATE_COLUMNS: &str = "id, path, title, kind, \
    size_bytes, duration_ms, container, bitrate_bps, video_codec, audio_codec, \
    width, height, frame_rate_mille, audio_channels, sample_rate, \
    series_name, season_number, episode_number, subtitle_tracks_json, \
    artist, album, album_artist, genre, created_at, chapters_json, \
    video_profile, video_level, pixel_format, color_primaries, color_transfer, color_space, \
    audio_tracks_json, file_mtime, file_size_seen, last_scanned, last_seen_scan_id, fingerprint, \
    community_rating, critic_rating, official_rating, production_year, premiere_date, \
    overview, tagline, provider_ids, series_folder, series_year, library_id, \
    title_fold, attachments_json, probe_schema_version";

#[derive(sqlx::FromRow)]
struct MediaItemRow {
    id: i64,
    path: String,
    title: String,
    kind: String,
    size_bytes: Option<i64>,
    duration_ms: Option<i64>,
    container: Option<String>,
    bitrate_bps: Option<i64>,
    video_codec: Option<String>,
    audio_codec: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    frame_rate_mille: Option<i64>,
    audio_channels: Option<i64>,
    sample_rate: Option<i64>,
    series_name: Option<String>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    subtitle_tracks_json: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    created_at: Option<i64>,
    chapters_json: Option<String>,
    video_profile: Option<String>,
    video_level: Option<i64>,
    pixel_format: Option<String>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    audio_tracks_json: Option<String>,
    file_mtime: Option<i64>,
    file_size_seen: Option<i64>,
    last_scanned: Option<i64>,
    last_seen_scan_id: Option<i64>,
    fingerprint: Option<Vec<u8>>,
    community_rating: Option<f64>,
    critic_rating: Option<f64>,
    official_rating: Option<String>,
    production_year: Option<i64>,
    premiere_date: Option<i64>,
    overview: Option<String>,
    tagline: Option<String>,
    provider_ids: Option<String>,
    series_folder: Option<String>,
    series_year: Option<i64>,
    library_id: Option<i64>,
    title_fold: Option<String>,
    attachments_json: Option<String>,
    probe_schema_version: i64,
}

/// media_items can be ~15k rows in a real deployment; each row is bound
/// individually inside the one enclosing transaction rather than built as
/// a giant multi-row VALUES list, which keeps the per-row bind list a
/// manageable fixed 51-placeholder statement while still batching the
/// whole copy as a single round of prepared-statement reuse.
async fn copy_media_items(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let select_sql = format!("SELECT {MEDIA_MIGRATE_COLUMNS} FROM media_items ORDER BY id");
    let rows: Vec<MediaItemRow> = sqlx::query_as(&select_sql)
        .fetch_all(sp)
        .await
        .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    let insert_sql = format!(
        "INSERT INTO media_items ({MEDIA_MIGRATE_COLUMNS}) VALUES \
         ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,\
         $24,$25,$26,$27,$28,$29,$30,$31,$32,$33,$34,$35,$36,$37,$38,$39,$40,$41,$42,$43,$44,\
         $45,$46,$47,$48,$49,$50,$51)"
    );
    for row in rows {
        sqlx::query(&insert_sql)
            .bind(row.id)
            .bind(row.path)
            .bind(row.title)
            .bind(row.kind)
            .bind(row.size_bytes)
            .bind(row.duration_ms)
            .bind(row.container)
            .bind(row.bitrate_bps)
            .bind(row.video_codec)
            .bind(row.audio_codec)
            .bind(row.width)
            .bind(row.height)
            .bind(row.frame_rate_mille)
            .bind(row.audio_channels)
            .bind(row.sample_rate)
            .bind(row.series_name)
            .bind(row.season_number)
            .bind(row.episode_number)
            .bind(row.subtitle_tracks_json)
            .bind(row.artist)
            .bind(row.album)
            .bind(row.album_artist)
            .bind(row.genre)
            .bind(row.created_at)
            .bind(row.chapters_json)
            .bind(row.video_profile)
            .bind(row.video_level)
            .bind(row.pixel_format)
            .bind(row.color_primaries)
            .bind(row.color_transfer)
            .bind(row.color_space)
            .bind(row.audio_tracks_json)
            .bind(row.file_mtime)
            .bind(row.file_size_seen)
            .bind(row.last_scanned)
            .bind(row.last_seen_scan_id)
            .bind(row.fingerprint)
            .bind(row.community_rating)
            .bind(row.critic_rating)
            .bind(row.official_rating)
            .bind(row.production_year)
            .bind(row.premiere_date)
            .bind(row.overview)
            .bind(row.tagline)
            .bind(row.provider_ids)
            .bind(row.series_folder)
            .bind(row.series_year)
            .bind(row.library_id)
            .bind(row.title_fold)
            .bind(row.attachments_json)
            .bind(row.probe_schema_version)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

// ---------------------------------------------------------------------
// Dependents
// ---------------------------------------------------------------------

async fn copy_auth_tokens(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(String, Vec<u8>, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT token_hash, user_id, device_id, created_at, expires_at FROM auth_tokens",
    )
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (token_hash, user_id, device_id, created_at, expires_at) in rows {
        sqlx::query(
            "INSERT INTO auth_tokens (token_hash, user_id, device_id, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(device_id)
        .bind(created_at)
        .bind(expires_at)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_user_data(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(Vec<u8>, i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT user_id, item_id, played, play_count, last_played_position_ticks, \
         is_favorite, last_played_at FROM user_data",
    )
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (user_id, item_id, played, play_count, ticks, is_favorite, last_played_at) in rows {
        sqlx::query(
            "INSERT INTO user_data (user_id, item_id, played, play_count, \
             last_played_position_ticks, is_favorite, last_played_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(user_id)
        .bind(item_id)
        .bind(played)
        .bind(play_count)
        .bind(ticks)
        .bind(is_favorite)
        .bind(last_played_at)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_user_configuration(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(Vec<u8>, String, i64)> =
        sqlx::query_as("SELECT user_id, config, updated_at FROM user_configuration")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (user_id, config, updated_at) in rows {
        sqlx::query(
            "INSERT INTO user_configuration (user_id, config, updated_at) VALUES ($1, $2, $3)",
        )
        .bind(user_id)
        .bind(config)
        .bind(updated_at)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_display_preferences(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(Vec<u8>, String, String, String, i64)> =
        sqlx::query_as("SELECT user_id, dp_id, client, prefs, updated_at FROM display_preferences")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (user_id, dp_id, client, prefs, updated_at) in rows {
        sqlx::query(
            "INSERT INTO display_preferences (user_id, dp_id, client, prefs, updated_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(user_id)
        .bind(dp_id)
        .bind(client)
        .bind(prefs)
        .bind(updated_at)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_artwork(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, String, String, String)> =
        sqlx::query_as("SELECT item_id, role, source, locator FROM artwork")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (item_id, role, source, locator) in rows {
        sqlx::query("INSERT INTO artwork (item_id, role, source, locator) VALUES ($1, $2, $3, $4)")
            .bind(item_id)
            .bind(role)
            .bind(source)
            .bind(locator)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

/// Shared shape for the three `(item_id, <entity>_id)` many-to-many join
/// tables (item_genres / item_studios / item_tags).
async fn copy_item_link(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    entity_col: &str,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, i64)> =
        sqlx::query_as(&format!("SELECT item_id, {entity_col} FROM {table}"))
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (item_id, entity_id) in rows {
        sqlx::query(&format!(
            "INSERT INTO {table} (item_id, {entity_col}) VALUES ($1, $2)"
        ))
        .bind(item_id)
        .bind(entity_id)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_item_people(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, i64, String, Option<String>, String, Option<i64>)> = sqlx::query_as(
        "SELECT item_id, person_id, role, character, person_kind, sort_order FROM item_people",
    )
    .fetch_all(sp)
    .await
    .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (item_id, person_id, role, character, person_kind, sort_order) in rows {
        sqlx::query(
            "INSERT INTO item_people \
             (item_id, person_id, role, character, person_kind, sort_order) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(item_id)
        .bind(person_id)
        .bind(role)
        .bind(character)
        .bind(person_kind)
        .bind(sort_order)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_collection_items(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, i64, i64)> =
        sqlx::query_as("SELECT collection_id, item_id, sort_order FROM collection_items")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (collection_id, item_id, sort_order) in rows {
        sqlx::query(
            "INSERT INTO collection_items (collection_id, item_id, sort_order) \
             VALUES ($1, $2, $3)",
        )
        .bind(collection_id)
        .bind(item_id)
        .bind(sort_order)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}

async fn copy_playlist_items(
    sp: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, StoreError> {
    let rows: Vec<(i64, String, i64, i64)> =
        sqlx::query_as("SELECT playlist_id, entry_id, item_id, sort_order FROM playlist_items")
            .fetch_all(sp)
            .await
            .map_err(StoreError::Sqlx)?;
    let n = rows.len() as u64;
    for (playlist_id, entry_id, item_id, sort_order) in rows {
        sqlx::query(
            "INSERT INTO playlist_items (playlist_id, entry_id, item_id, sort_order) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(playlist_id)
        .bind(entry_id)
        .bind(item_id)
        .bind(sort_order)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Sqlx)?;
    }
    Ok(n)
}
