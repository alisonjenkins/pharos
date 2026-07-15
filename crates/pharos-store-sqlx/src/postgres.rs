//! Postgres backend mirroring the sqlite store (T39).
//!
//! Schema lives under `migrations/postgres/`. The trait surface is
//! identical to `SqliteStore` — handlers swap by changing the
//! `state::Stores` type alias and the `[database].url` scheme. Phase 1
//! does not run an integration suite here (real postgres + dockerized
//! CI lands with T37 phase 2); the impl is exercised by `cargo build
//! --features postgres` in CI.
//!
//! Differences from sqlite worth flagging:
//! - Numeric primary keys are `BIGINT` (i64) instead of sqlite
//!   `INTEGER`; `u64 → i64` conversion uses `try_from` (overflow
//!   reported as `DomainError::Backend`).
//! - `users.id` is `BYTEA` for the 16-byte UUID — same as sqlite's
//!   `BLOB`.
//! - `ON CONFLICT` upsert syntax matches sqlite's; postgres supports
//!   both (`ON CONFLICT (col) DO UPDATE`).

use crate::StoreError;
use pharos_core::{
    AuthError, AuthResult, AuthToken, Collection, CollectionCount, CollectionStore, DomainError,
    DomainResult, Fingerprint, Genre, GenreCount, GenreStore, ItemPerson, Library, LibraryKind,
    LibraryStore, MediaId, MediaItem, MediaKind, MediaMetadata, MediaProbe, MediaQuery, MediaStore,
    PersistedSyncGroup, PersistedTranscodeSession, Person, PersonCount, PersonKind, PersonRef,
    PersonStore, Playlist, PlaylistEntry, PlaylistStore, PreferenceStore, ScanState, SecretString,
    SeriesInfo, Studio, StudioCount, StudioStore, SyncGroupStore, Tag, TagCount, TagStore,
    TokenStore, TranscodeSessionStore, UserDataStore, UserId, UserItemData, UserPolicy, UserRecord,
    UserStore,
};

const MEDIA_COLUMNS: &str = "id, path, title, kind, size_bytes, duration_ms, container, \
    bitrate_bps, video_codec, audio_codec, width, height, frame_rate_mille, \
    audio_channels, sample_rate, series_name, season_number, episode_number, \
    subtitle_tracks_json, attachments_json, artist, album, album_artist, genre, created_at, chapters_json, \
    video_profile, video_level, pixel_format, color_primaries, color_transfer, color_space, \
    audio_tracks_json, community_rating, critic_rating, official_rating, production_year, \
    premiere_date, overview, tagline, provider_ids, production_locations_json, trailers_json, \
    series_folder, series_year, track_number, disc_number, release_year";
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

/// LIB-B4 — `MEDIA_COLUMNS` with each column qualified by a table alias,
/// for the search join (so the FromRow still maps the plain column names).
fn media_columns_prefixed_pg(alias: &str) -> String {
    MEDIA_COLUMNS
        .split(',')
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| format!("{alias}.{c} AS {c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

fn map_sqlx<E: std::fmt::Display>(e: E) -> AuthError {
    AuthError::Backend(e.to_string())
}

fn media_id_i64(id: MediaId) -> DomainResult<i64> {
    i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl PostgresStore {
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// LIB-B1 — total matching rows for `q` BEFORE LIMIT/OFFSET, the
    /// empty-page fallback for `query()`. Mirrors
    /// `SqliteStore::query_count`.
    async fn query_count(&self, q: &pharos_core::MediaQuery) -> DomainResult<u64> {
        use crate::media_query::{self, Param};
        let mut counting = q.clone();
        counting.limit = None;
        counting.start_index = 0;
        counting.sort = Vec::new();
        let user = media_query::user_data_user(&counting);
        let join_active = media_query::needs_user_data_join(&counting);
        let offset = usize::from(join_active);
        let built = media_query::build(&counting, |n| format!("${}", n + offset), "ALL");
        let join = if join_active {
            "LEFT JOIN user_data ud ON ud.item_id = media_items.id AND ud.user_id = $1"
        } else {
            ""
        };
        let where_clause = if built.where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", built.where_sql)
        };
        let sql = format!("SELECT COUNT(*) FROM media_items {join} {where_clause}");
        let mut query = sqlx::query_as::<_, (i64,)>(&sql);
        if let Some(uid) = user {
            query = query.bind(uid.0.as_bytes().to_vec());
        }
        for p in &built.params {
            query = match p {
                Param::Text(s) => query.bind(s.clone()),
                Param::Int(i) => query.bind(*i),
            };
        }
        let (count,) = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(u64::try_from(count.max(0)).unwrap_or(0))
    }

    /// LIB-B4 — the FTS (`search_tsv @@ to_tsquery`) ∪ substring
    /// `hit(rid, score)` subquery shared by the postgres search page +
    /// total. `score` is `ts_rank` for FTS hits (HIGHER = better; ORDER BY
    /// score DESC) and `-1` for substring-only hits so the ranked FTS hits
    /// sort first. Placeholders: `$1` = tsquery text, `$2` = LIKE needle
    /// (reused for title + overview). `MAX(score)` keeps a row's best arm.
    fn search_hit_subquery_pg() -> &'static str {
        "SELECT rid, MAX(score) AS score FROM ( \
             SELECT m1.id AS rid, ts_rank(m1.search_tsv, to_tsquery('simple', $1)) AS score \
             FROM media_items m1 WHERE m1.search_tsv @@ to_tsquery('simple', $1) \
             UNION ALL \
             SELECT m2.id AS rid, -1::real AS score FROM media_items m2 \
             WHERE (LOWER(m2.title) LIKE $2 OR LOWER(COALESCE(m2.overview, '')) LIKE $2) \
         ) hits GROUP BY rid"
    }

    /// LIB-B4 — one ranked page of postgres search hits.
    async fn search_page_pg(
        &self,
        tsquery: &str,
        needle: &str,
        kinds: &[MediaKind],
        limit: i64,
        offset: i64,
    ) -> DomainResult<Vec<MediaItem>> {
        // $1 tsquery, $2 needle are consumed by the hit subquery; kinds
        // start at $3, then limit/offset.
        let mut next = 3usize;
        let kind_clause = if kinds.is_empty() {
            String::new()
        } else {
            let holes: Vec<String> = kinds
                .iter()
                .map(|_| {
                    let p = format!("${next}");
                    next += 1;
                    p
                })
                .collect();
            format!("AND m.kind IN ({})", holes.join(", "))
        };
        let limit_ph = format!("${next}");
        next += 1;
        let offset_ph = format!("${next}");
        let sql = format!(
            "SELECT {cols} FROM ({hit}) hit \
             JOIN media_items m ON m.id = hit.rid \
             WHERE TRUE {kind_clause} \
             ORDER BY hit.score DESC, m.id ASC LIMIT {limit_ph} OFFSET {offset_ph}",
            cols = media_columns_prefixed_pg("m"),
            hit = Self::search_hit_subquery_pg(),
        );
        let mut query = sqlx::query_as::<_, MediaRow>(&sql)
            .bind(tsquery)
            .bind(needle);
        for k in kinds {
            query = query.bind(k.as_str());
        }
        query = query.bind(limit).bind(offset);
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        rows.into_iter().map(MediaRow::into_domain).collect()
    }

    /// LIB-B4 — total distinct postgres search hits BEFORE limit/offset.
    async fn search_total_pg(
        &self,
        tsquery: &str,
        needle: &str,
        kinds: &[MediaKind],
    ) -> DomainResult<u64> {
        let mut next = 3usize;
        let kind_clause = if kinds.is_empty() {
            String::new()
        } else {
            let holes: Vec<String> = kinds
                .iter()
                .map(|_| {
                    let p = format!("${next}");
                    next += 1;
                    p
                })
                .collect();
            format!("AND m.kind IN ({})", holes.join(", "))
        };
        let sql = format!(
            "SELECT COUNT(*) FROM ({hit}) hit \
             JOIN media_items m ON m.id = hit.rid WHERE TRUE {kind_clause}",
            hit = Self::search_hit_subquery_pg(),
        );
        let mut query = sqlx::query_as::<_, (i64,)>(&sql).bind(tsquery).bind(needle);
        for k in kinds {
            query = query.bind(k.as_str());
        }
        let (count,) = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(u64::try_from(count.max(0)).unwrap_or(0))
    }

    /// LIB-B5 — aggregate facet counts over the base query's WHERE scope
    /// (postgres mirror of `SqliteStore::facets_impl`).
    async fn facets_impl(
        &self,
        base: &pharos_core::MediaQuery,
        req: &pharos_core::FacetRequest,
    ) -> DomainResult<pharos_core::MediaFacets> {
        use crate::media_query::{self, Param};
        use pharos_core::{FacetValue, MediaFacets};
        let mut out = MediaFacets::default();
        if !req.is_any() {
            return Ok(out);
        }
        let mut scope = base.clone();
        scope.sort = Vec::new();
        scope.limit = None;
        scope.start_index = 0;
        let user = media_query::user_data_user(&scope);
        let join_active = media_query::needs_user_data_join(&scope);
        // The user-data join binds $1; the base builder placeholders start at
        // $2 (offset by one). Each facet statement appends NO further params,
        // so the builder offset is the only adjustment.
        let base_offset = usize::from(join_active);
        let built = media_query::build(&scope, |n| format!("${}", n + base_offset), "ALL");
        let join = if join_active {
            "LEFT JOIN user_data ud ON ud.item_id = media_items.id AND ud.user_id = $1"
        } else {
            ""
        };
        let where_clause = if built.where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", built.where_sql)
        };
        let matched = format!("SELECT media_items.id FROM media_items {join} {where_clause}");

        async fn run_facet(
            pool: &PgPool,
            sql: &str,
            user: Option<UserId>,
            params: &[Param],
        ) -> DomainResult<Vec<FacetValue>> {
            let mut q = sqlx::query_as::<_, (String, String, i64)>(sql);
            if let Some(uid) = user {
                q = q.bind(uid.0.as_bytes().to_vec());
            }
            for p in params {
                q = match p {
                    Param::Text(s) => q.bind(s.clone()),
                    Param::Int(i) => q.bind(*i),
                };
            }
            let rows = q
                .fetch_all(pool)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            Ok(rows
                .into_iter()
                .map(|(value, wire_id, c)| FacetValue {
                    value,
                    wire_id,
                    count: c.max(0) as u32,
                })
                .collect())
        }

        let entity_facets: &[(bool, &str, &str)] = &[
            (req.genres, "item_genres", "genre_id"),
            (req.studios, "item_studios", "studio_id"),
            (req.tags, "item_tags", "tag_id"),
            (req.people, "item_people", "person_id"),
        ];
        let entity_tables: &[&str] = &["genres", "studios", "tags", "people"];
        for (idx, (want, join_tbl, join_col)) in entity_facets.iter().enumerate() {
            if !*want {
                continue;
            }
            let entity = entity_tables[idx];
            let sql = format!(
                "SELECT e.name, e.wire_id, COUNT(DISTINCT j.item_id) AS c \
                 FROM {entity} e JOIN {join_tbl} j ON j.{join_col} = e.id \
                 WHERE j.item_id IN ({matched}) \
                 GROUP BY e.id, e.name, e.wire_id \
                 ORDER BY c DESC, e.name ASC"
            );
            let vals = run_facet(&self.pool, &sql, user, &built.params).await?;
            match entity {
                "genres" => out.genres = vals,
                "studios" => out.studios = vals,
                "tags" => out.tags = vals,
                "people" => out.people = vals,
                _ => {}
            }
        }

        if req.years {
            let sql = format!(
                "SELECT production_year::text, production_year::text, COUNT(*) AS c \
                 FROM media_items WHERE production_year IS NOT NULL \
                 AND id IN ({matched}) GROUP BY production_year \
                 ORDER BY production_year DESC"
            );
            out.years = run_facet(&self.pool, &sql, user, &built.params).await?;
        }
        if req.official_ratings {
            let sql = format!(
                "SELECT official_rating, official_rating, COUNT(*) AS c \
                 FROM media_items WHERE official_rating IS NOT NULL AND official_rating <> '' \
                 AND id IN ({matched}) GROUP BY official_rating \
                 ORDER BY c DESC, official_rating ASC"
            );
            out.official_ratings = run_facet(&self.pool, &sql, user, &built.params).await?;
        }
        Ok(out)
    }
}

impl crate::ServerConfigStore for PostgresStore {
    /// Read or initialise this server's stable identity UUID. Mirrors
    /// `SqliteStore::load_or_create_server_id` (T35).
    async fn load_or_create_server_id(&self) -> Result<String, StoreError> {
        if let Some((id,)) =
            sqlx::query_as::<_, (String,)>("SELECT server_id FROM system_identity WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?
        {
            return Ok(id);
        }
        let new_id = Uuid::new_v4().simple().to_string();
        let now = now_unix_secs();
        match sqlx::query(
            "INSERT INTO system_identity (id, server_id, created_at) VALUES (1, $1, $2)",
        )
        .bind(&new_id)
        .bind(now)
        .execute(&self.pool)
        .await
        {
            Ok(_) => Ok(new_id),
            Err(sqlx::Error::Database(_)) => {
                let (id,) = sqlx::query_as::<_, (String,)>(
                    "SELECT server_id FROM system_identity WHERE id = 1",
                )
                .fetch_one(&self.pool)
                .await?;
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn load_runtime_config(&self) -> Result<crate::RuntimeConfig, StoreError> {
        let row = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            "SELECT server_name, login_disclaimer, custom_css FROM runtime_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((s, l, c)) => crate::RuntimeConfig {
                server_name: s,
                login_disclaimer: l,
                custom_css: c,
            },
            None => crate::RuntimeConfig::default(),
        })
    }

    async fn set_runtime_config(&self, cfg: &crate::RuntimeConfig) -> Result<(), StoreError> {
        let now = now_unix_secs();
        sqlx::query(
            "INSERT INTO runtime_config (id, server_name, login_disclaimer, custom_css, updated_at)
             VALUES (1, $1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET
                 server_name = EXCLUDED.server_name,
                 login_disclaimer = EXCLUDED.login_disclaimer,
                 custom_css = EXCLUDED.custom_css,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&cfg.server_name)
        .bind(&cfg.login_disclaimer)
        .bind(&cfg.custom_css)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// T69 — rename a library by `wire_id` in place (see the sqlite twin).
    async fn rename_library(&self, wire_id: &str, new_name: &str) -> Result<u64, StoreError> {
        let res = sqlx::query("UPDATE libraries SET name = $1 WHERE wire_id = $2")
            .bind(new_name)
            .bind(wire_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// T72 — read a persisted named-configuration section blob by key (see
    /// the sqlite twin). `None` when the section has never been written.
    async fn load_named_config(&self, key: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query_as::<_, (String,)>("SELECT value FROM named_config WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(v,)| v))
    }

    /// T72 — upsert a named-configuration section blob (see the sqlite twin).
    async fn set_named_config(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let now = now_unix_secs();
        sqlx::query(
            "INSERT INTO named_config (key, value, updated_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (key) DO UPDATE SET
                 value = EXCLUDED.value,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// LIB-B2 — distinct `(series_folder, series_name)` keys (see the sqlite
    /// twin). Used by the API to resolve a `?ParentId=<series synth id>`.
    async fn distinct_series_keys(&self) -> Result<Vec<(Option<String>, String)>, StoreError> {
        let rows = sqlx::query_as::<_, (Option<String>, String)>(
            "SELECT DISTINCT series_folder, series_name FROM media_items \
             WHERE series_name IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// LIB-B2 — distinct `(series_folder, series_name, season_number)` keys.
    async fn distinct_season_keys(&self) -> Result<Vec<(Option<String>, String, i64)>, StoreError> {
        // season_number is INTEGER (INT4) in the postgres schema — decoding
        // it as i64 aborts the whole query ("mismatched types … INT4"),
        // which 500'd every /Items?ParentId=<season> (B19). Decode i32,
        // widen after.
        let rows = sqlx::query_as::<_, (Option<String>, String, i32)>(
            "SELECT DISTINCT series_folder, series_name, season_number FROM media_items \
             WHERE series_name IS NOT NULL AND season_number IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(f, n, sn)| (f, n, i64::from(sn)))
            .collect())
    }

    /// LIB-B2 — distinct non-empty artist + album_artist names.
    async fn distinct_artist_names(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT artist AS n FROM media_items WHERE artist IS NOT NULL AND artist <> '' \
             UNION SELECT DISTINCT album_artist FROM media_items \
             WHERE album_artist IS NOT NULL AND album_artist <> ''",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    /// LIB-B2 — distinct non-empty album names.
    async fn distinct_album_names(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT album FROM media_items WHERE album IS NOT NULL AND album <> ''",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    /// LIB-B2 — distinct raw `genre` probe strings (legacy ParentId=genre
    /// fallback; see the sqlite twin).
    async fn distinct_genre_fields(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT genre FROM media_items WHERE genre IS NOT NULL AND genre <> ''",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }
}

impl MediaStore for PostgresStore {
    #[tracing::instrument(skip(self), fields(media.id = %id))]
    async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
        let id_i64 = media_id_i64(id)?;
        let sql = format!("SELECT {MEDIA_COLUMNS} FROM media_items WHERE id = $1");
        let row = sqlx::query_as::<_, MediaRow>(&sql)
            .bind(id_i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        match row {
            Some(r) => r.into_domain(),
            None => Err(DomainError::NotFound(id)),
        }
    }

    #[tracing::instrument(skip(self, item), fields(media.id = %item.id, media.kind = item.kind.as_str()))]
    async fn put(&self, item: MediaItem) -> DomainResult<()> {
        let id_i64 = media_id_i64(item.id)?;
        let path = item
            .path
            .to_str()
            .ok_or_else(|| DomainError::Backend("non-utf8 path".into()))?;
        let p = &item.probe;
        let series_name = item.series.as_ref().map(|s| s.series_name.as_str());
        let season_number = item.series.as_ref().and_then(|s| s.season_number);
        let episode_number = item.series.as_ref().and_then(|s| s.episode_number);
        // LIB-C11 — show-folder identity + parsed year.
        let series_folder = item
            .series
            .as_ref()
            .and_then(|s| s.series_folder.as_deref());
        let series_year = item.series.as_ref().and_then(|s| s.series_year);
        let subtitle_tracks_json = crate::subtitle_track_json::encode(&p.subtitle_tracks);
        let attachments_json = crate::attachment_json::encode(&p.attachments);
        let chapters_json = crate::chapter_json::encode(&p.chapters);
        let m = &item.metadata;
        let provider_ids_json = crate::provider_ids_json::encode(&m.provider_ids);
        let production_locations_json = crate::string_list_json::encode(&m.production_locations);
        let trailers_json = crate::string_list_json::encode(&m.trailers);
        let created_at = item.created_at.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
        sqlx::query(
            "INSERT INTO media_items (id, path, title, kind, \
                size_bytes, duration_ms, container, bitrate_bps, \
                video_codec, audio_codec, width, height, frame_rate_mille, \
                audio_channels, sample_rate, \
                series_name, season_number, episode_number, \
                subtitle_tracks_json, \
                artist, album, album_artist, genre, created_at, chapters_json, \
                video_profile, video_level, \
                pixel_format, color_primaries, color_transfer, color_space, \
                audio_tracks_json, \
                community_rating, critic_rating, official_rating, production_year, \
                premiere_date, overview, tagline, provider_ids, \
                series_folder, series_year, title_fold, attachments_json, \
                track_number, disc_number, release_year, \
                production_locations_json, trailers_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, \
                     $28, $29, $30, $31, $32, \
                     $33, $34, $35, $36, $37, $38, $39, $40, $41, $42, $43, $44, $45, $46, $47, \
                     $48, $49)
             ON CONFLICT (id) DO UPDATE SET path = EXCLUDED.path,
                                            title = EXCLUDED.title,
                                            title_fold = EXCLUDED.title_fold,
                                            kind = EXCLUDED.kind,
                                            size_bytes = EXCLUDED.size_bytes,
                                            duration_ms = EXCLUDED.duration_ms,
                                            container = EXCLUDED.container,
                                            bitrate_bps = EXCLUDED.bitrate_bps,
                                            video_codec = EXCLUDED.video_codec,
                                            audio_codec = EXCLUDED.audio_codec,
                                            width = EXCLUDED.width,
                                            height = EXCLUDED.height,
                                            frame_rate_mille = EXCLUDED.frame_rate_mille,
                                            audio_channels = EXCLUDED.audio_channels,
                                            sample_rate = EXCLUDED.sample_rate,
                                            series_name = EXCLUDED.series_name,
                                            season_number = EXCLUDED.season_number,
                                            episode_number = EXCLUDED.episode_number,
                                            subtitle_tracks_json = EXCLUDED.subtitle_tracks_json,
                                            artist = EXCLUDED.artist,
                                            album = EXCLUDED.album,
                                            album_artist = EXCLUDED.album_artist,
                                            genre = EXCLUDED.genre,
                                            chapters_json = EXCLUDED.chapters_json,
                                            video_profile = EXCLUDED.video_profile,
                                            video_level = EXCLUDED.video_level,
                                            pixel_format = EXCLUDED.pixel_format,
                                            color_primaries = EXCLUDED.color_primaries,
                                            color_transfer = EXCLUDED.color_transfer,
                                            color_space = EXCLUDED.color_space,
                                            audio_tracks_json = EXCLUDED.audio_tracks_json,
                                            community_rating = EXCLUDED.community_rating,
                                            critic_rating = EXCLUDED.critic_rating,
                                            official_rating = EXCLUDED.official_rating,
                                            production_year = EXCLUDED.production_year,
                                            premiere_date = EXCLUDED.premiere_date,
                                            overview = EXCLUDED.overview,
                                            tagline = EXCLUDED.tagline,
                                            provider_ids = EXCLUDED.provider_ids,
                                            series_folder = EXCLUDED.series_folder,
                                            series_year = EXCLUDED.series_year,
                                            attachments_json = EXCLUDED.attachments_json,
                                            track_number = EXCLUDED.track_number,
                                            disc_number = EXCLUDED.disc_number,
                                            release_year = EXCLUDED.release_year,
                                            production_locations_json = EXCLUDED.production_locations_json,
                                            trailers_json = EXCLUDED.trailers_json,
                                            created_at = COALESCE(media_items.created_at, EXCLUDED.created_at)",
        )
        .bind(id_i64)
        .bind(path)
        .bind(&item.title)
        .bind(item.kind.as_str())
        .bind(p.size_bytes.map(|v| v as i64))
        .bind(p.duration_ms.map(|v| v as i64))
        .bind(p.container.as_deref())
        .bind(p.bitrate_bps.map(|v| v as i64))
        .bind(p.video_codec.as_deref())
        .bind(p.audio_codec.as_deref())
        .bind(p.width.map(|v| v as i32))
        .bind(p.height.map(|v| v as i32))
        .bind(p.frame_rate_mille.map(|v| v as i32))
        .bind(p.audio_channels.map(|v| v as i32))
        .bind(p.sample_rate.map(|v| v as i32))
        .bind(series_name)
        .bind(season_number.map(|v| v as i32))
        .bind(episode_number.map(|v| v as i32))
        .bind(subtitle_tracks_json)
        .bind(p.artist.as_deref())
        .bind(p.album.as_deref())
        .bind(p.album_artist.as_deref())
        .bind(p.genre.as_deref())
        .bind(created_at)
        .bind(chapters_json)
        .bind(p.video_profile.as_deref())
        .bind(p.video_level.map(|v| v as i32))
        .bind(p.pixel_format.as_deref())
        .bind(p.color_primaries.as_deref())
        .bind(p.color_transfer.as_deref())
        .bind(p.color_space.as_deref())
        .bind(crate::audio_track_json::encode(&p.audio_tracks))
        .bind(m.community_rating)
        .bind(m.critic_rating)
        .bind(m.official_rating.as_deref())
        .bind(m.production_year.map(|v| v as i32))
        .bind(m.premiere_date)
        .bind(m.overview.as_deref())
        .bind(m.tagline.as_deref())
        .bind(provider_ids_json)
        .bind(series_folder)
        .bind(series_year.map(|v| v as i32))
        // LIB-B2 — Unicode-case-folded title for SQL search + SortName.
        .bind(item.title.to_lowercase())
        .bind(attachments_json)
        .bind(p.track_number.map(|v| v as i32))
        .bind(p.disc_number.map(|v| v as i32))
        .bind(p.year.map(|v| v as i32))
        .bind(production_locations_json)
        .bind(trailers_json)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn list(&self) -> DomainResult<Vec<MediaItem>> {
        let sql = format!("SELECT {MEDIA_COLUMNS} FROM media_items ORDER BY id");
        let rows = sqlx::query_as::<_, MediaRow>(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        rows.into_iter().map(MediaRow::into_domain).collect()
    }

    #[tracing::instrument(skip(self, q))]
    async fn query(&self, q: &MediaQuery) -> DomainResult<(Vec<MediaItem>, u64)> {
        use crate::media_query::{self, Param};
        let user = media_query::user_data_user(q);
        let join_active = media_query::needs_user_data_join(q);
        // Postgres uses `$N`. When the user-data join is present its user id
        // binds as `$1`, so every builder placeholder is offset by one.
        let offset = usize::from(join_active);
        let built = media_query::build(q, |n| format!("${}", n + offset), "ALL");
        let join = if join_active {
            "LEFT JOIN user_data ud ON ud.item_id = media_items.id AND ud.user_id = $1"
        } else {
            ""
        };
        let where_clause = if built.where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", built.where_sql)
        };
        let limit_clause = built.limit_sql;
        let sql = format!(
            "SELECT {MEDIA_COLUMNS}, COUNT(*) OVER () AS total_count \
             FROM media_items {join} {where_clause} ORDER BY {} {limit_clause}",
            built.order_sql,
        );
        let mut query = sqlx::query_as::<_, QueryRow>(&sql);
        if let Some(uid) = user {
            query = query.bind(uid.0.as_bytes().to_vec());
        }
        for p in &built.params {
            query = match p {
                Param::Text(s) => query.bind(s.clone()),
                Param::Int(i) => query.bind(*i),
            };
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let total = match rows.first() {
            Some(r) => u64::try_from(r.total_count.max(0)).unwrap_or(0),
            None => self.query_count(q).await?,
        };
        let items = rows
            .into_iter()
            .map(|r| r.media.into_domain())
            .collect::<DomainResult<Vec<_>>>()?;
        Ok((items, total))
    }

    #[tracing::instrument(skip(self, q))]
    async fn search(&self, q: &pharos_core::SearchQuery) -> DomainResult<(Vec<MediaItem>, u64)> {
        let tokens = pharos_core::search_tokens(&q.term);
        if tokens.is_empty() {
            return Ok((Vec::new(), 0));
        }
        // to_tsquery('simple', $1) input: prefix-marked, AND-joined tokens.
        // Tokens are alphanumeric-only (search_tokens), so no tsquery
        // operator can leak; bound as a single parameter, never interpolated.
        let tsquery = tokens
            .iter()
            .map(|t| format!("{t}:*"))
            .collect::<Vec<_>>()
            .join(" & ");
        let needle = format!(
            "%{}%",
            crate::media_query::like_escape(&q.term.trim().to_lowercase())
        );
        let limit = i64::from(q.limit.max(1));
        let offset = i64::from(q.offset);
        let total = self.search_total_pg(&tsquery, &needle, &q.kinds).await?;
        let rows = self
            .search_page_pg(&tsquery, &needle, &q.kinds, limit, offset)
            .await?;
        Ok((rows, total))
    }

    #[tracing::instrument(skip(self, base, req))]
    async fn facets(
        &self,
        base: &pharos_core::MediaQuery,
        req: &pharos_core::FacetRequest,
    ) -> DomainResult<pharos_core::MediaFacets> {
        self.facets_impl(base, req).await
    }

    #[tracing::instrument(skip(self), fields(media.id = %id))]
    async fn scan_state(&self, id: MediaId) -> DomainResult<Option<ScanState>> {
        let id_i64 = media_id_i64(id)?;
        let row = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
            "SELECT last_scanned, file_mtime, file_size_seen, last_seen_scan_id, probe_schema_version \
             FROM media_items WHERE id = $1",
        )
        .bind(id_i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(last_scanned, file_mtime, file_size, last_seen, schema_version)| ScanState {
                last_scanned: last_scanned.unwrap_or(0),
                file_mtime: file_mtime.unwrap_or(0),
                file_size: file_size.and_then(|v| u64::try_from(v).ok()).unwrap_or(0),
                last_seen_scan_id: last_seen.unwrap_or(0),
                probe_schema_version: schema_version.unwrap_or(0),
            },
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn begin_scan(&self, root: &std::path::Path) -> DomainResult<i64> {
        let root_str = root.to_str();
        let now = now_unix_secs();
        let row = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO scan_runs (root, started_at) VALUES ($1, $2) RETURNING id",
        )
        .bind(root_str)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.0)
    }

    #[tracing::instrument(skip(self), fields(media.id = %id, scan.id = scan_id))]
    async fn mark_seen(
        &self,
        id: MediaId,
        scan_id: i64,
        mtime: i64,
        size: u64,
    ) -> DomainResult<()> {
        let id_i64 = media_id_i64(id)?;
        let size_i64 =
            i64::try_from(size).map_err(|e| DomainError::Backend(format!("size overflow: {e}")))?;
        let now = now_unix_secs();
        // Stamp the current probe schema version (see the sqlite mark_seen).
        sqlx::query(
            "UPDATE media_items SET file_mtime = $1, file_size_seen = $2, \
             last_scanned = $3, last_seen_scan_id = $4, probe_schema_version = $5 WHERE id = $6",
        )
        .bind(mtime)
        .bind(size_i64)
        .bind(now)
        .bind(scan_id)
        .bind(pharos_core::PROBE_SCHEMA_VERSION)
        .bind(id_i64)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(scan.id = scan_id))]
    async fn sweep_unseen(&self, scan_id: i64, root_prefix: &str) -> DomainResult<Vec<MediaId>> {
        // Root-scoped, single atomic DELETE (V10). Path-separator boundary so a
        // sibling root sharing a string prefix is never swept; wildcards escaped.
        let like = crate::root_like_pattern(root_prefix);
        let rows = sqlx::query_as::<_, (i64,)>(
            "DELETE FROM media_items \
             WHERE (last_seen_scan_id IS NULL OR last_seen_scan_id != $1) \
               AND path LIKE $2 ESCAPE '\\' \
             RETURNING id",
        )
        .bind(scan_id)
        .bind(like)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        rows.into_iter()
            .map(|(id,)| {
                u64::try_from(id).map_err(|e| DomainError::Backend(format!("id negative: {e}")))
            })
            .collect()
    }

    #[tracing::instrument(skip(self), fields(scan.id = scan_id))]
    async fn finish_scan(
        &self,
        scan_id: i64,
        items_seen: i64,
        items_swept: i64,
    ) -> DomainResult<()> {
        let now = now_unix_secs();
        sqlx::query(
            "UPDATE scan_runs SET finished_at = $1, items_seen = $2, items_swept = $3 \
             WHERE id = $4",
        )
        .bind(now)
        .bind(items_seen)
        .bind(items_swept)
        .bind(scan_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self, fp))]
    async fn find_by_fp(&self, fp: Fingerprint) -> DomainResult<Option<MediaItem>> {
        // Raw 8 bytes bound as BYTEA; first match by ascending id.
        let sql = format!(
            "SELECT {MEDIA_COLUMNS} FROM media_items \
             WHERE fingerprint = $1 ORDER BY id LIMIT 1"
        );
        let row = sqlx::query_as::<_, MediaRow>(&sql)
            .bind(fp.as_slice())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        row.map(MediaRow::into_domain).transpose()
    }

    #[tracing::instrument(skip(self, fp), fields(media.id = %id))]
    async fn set_fingerprint(&self, id: MediaId, fp: Fingerprint) -> DomainResult<()> {
        let id_i64 = media_id_i64(id)?;
        sqlx::query("UPDATE media_items SET fingerprint = $1 WHERE id = $2")
            .bind(fp.as_slice())
            .bind(id_i64)
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(media.id = %id, media.path = %new_path.display()))]
    async fn rebind_path(&self, id: MediaId, new_path: &std::path::Path) -> DomainResult<()> {
        let id_i64 = media_id_i64(id)?;
        // UPDATE-only: keeps the id (and every user_data FK) intact, just
        // repoints the path of a moved/renamed file. Zero rows when absent.
        sqlx::query("UPDATE media_items SET path = $1 WHERE id = $2")
            .bind(new_path.to_string_lossy().as_ref())
            .bind(id_i64)
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self, locator), fields(media.id = %item_id, artwork.role = %role))]
    async fn set_artwork(
        &self,
        item_id: MediaId,
        role: &str,
        source: &str,
        locator: &str,
    ) -> DomainResult<()> {
        let id_i64 = media_id_i64(item_id)?;
        // One row per (item, role): a re-scan that discovers the same/winning
        // sidecar overwrites the locator + source rather than duplicating.
        sqlx::query(
            "INSERT INTO artwork (item_id, role, source, locator) VALUES ($1, $2, $3, $4) \
             ON CONFLICT(item_id, role) DO UPDATE SET source = excluded.source, \
                locator = excluded.locator",
        )
        .bind(id_i64)
        .bind(role)
        .bind(source)
        .bind(locator)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(media.id = %item_id))]
    async fn artwork_for(&self, item_id: MediaId) -> DomainResult<Vec<(String, String, String)>> {
        let id_i64 = media_id_i64(item_id)?;
        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT role, source, locator FROM artwork WHERE item_id = $1 ORDER BY role",
        )
        .bind(id_i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows)
    }
}

impl pharos_core::MediaSegmentStore for PostgresStore {
    async fn set_media_segments(
        &self,
        item_id: MediaId,
        segments: &[pharos_core::DetectedSegment],
        schema_version: i64,
    ) -> DomainResult<()> {
        let id = i64::try_from(item_id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        sqlx::query("DELETE FROM media_segments WHERE item_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for seg in segments {
            sqlx::query(
                "INSERT INTO media_segments (item_id, kind, start_ms, end_ms, detector, \
                    confidence, schema_version) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(id)
            .bind(seg.kind.as_str())
            .bind(seg.start_ms as i64)
            .bind(seg.end_ms as i64)
            .bind(&seg.detector)
            .bind(seg.confidence as f64)
            .bind(schema_version)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn media_segments_for(
        &self,
        item_id: MediaId,
    ) -> DomainResult<Vec<pharos_core::DetectedSegment>> {
        let id = i64::try_from(item_id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let rows = sqlx::query_as::<_, (String, i64, i64, String, f64)>(
            "SELECT kind, start_ms, end_ms, detector, confidence FROM media_segments \
             WHERE item_id = $1 ORDER BY start_ms",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|(k, s, e, det, c)| {
                pharos_core::MediaSegmentKind::from_str(&k).map(|kind| {
                    pharos_core::DetectedSegment {
                        kind,
                        start_ms: s.max(0) as u64,
                        end_ms: e.max(0) as u64,
                        detector: det,
                        confidence: c as f32,
                    }
                })
            })
            .collect())
    }

    async fn set_episode_fingerprint(
        &self,
        item_id: MediaId,
        kind: pharos_core::FingerprintKind,
        points: &[u32],
        schema_version: i64,
    ) -> DomainResult<()> {
        let id = i64::try_from(item_id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let bytes: Vec<u8> = points.iter().flat_map(|p| p.to_le_bytes()).collect();
        sqlx::query(
            "INSERT INTO episode_fingerprints (item_id, kind, points, schema_version) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (item_id, kind) DO UPDATE SET \
                points = EXCLUDED.points, schema_version = EXCLUDED.schema_version",
        )
        .bind(id)
        .bind(kind.as_str())
        .bind(bytes)
        .bind(schema_version)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn episode_fingerprint_for(
        &self,
        item_id: MediaId,
        kind: pharos_core::FingerprintKind,
        schema_version: i64,
    ) -> DomainResult<Option<Vec<u32>>> {
        let id = i64::try_from(item_id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let row = sqlx::query_as::<_, (Vec<u8>,)>(
            "SELECT points FROM episode_fingerprints WHERE item_id = $1 AND kind = $2 \
             AND schema_version = $3",
        )
        .bind(id)
        .bind(kind.as_str())
        .bind(schema_version)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(|(bytes,)| {
            bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        }))
    }
}

impl GenreStore for PostgresStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_genre(&self, name: &str) -> DomainResult<i64> {
        let wire_id = pharos_core::genre_wire_id(name);
        sqlx::query(
            "INSERT INTO genres (name, wire_id) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
        )
        .bind(name)
        .bind(&wire_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        // genres.id is INTEGER (i32) on postgres; widen to i64 for the
        // core signature (the join column item_id is BIGINT).
        let (id,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM genres WHERE name = $1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id as i64)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_genres(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 = media_id_i64(item)?;
        let mut wanted: Vec<String> = Vec::new();
        for n in names {
            let t = n.trim();
            if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                wanted.push(t.to_string());
            }
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        sqlx::query("DELETE FROM item_genres WHERE item_id = $1")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for name in &wanted {
            let wire_id = pharos_core::genre_wire_id(name);
            sqlx::query(
                "INSERT INTO genres (name, wire_id) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (gid,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM genres WHERE name = $1")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO item_genres (item_id, genre_id) VALUES ($1, $2) \
                 ON CONFLICT(item_id, genre_id) DO NOTHING",
            )
            .bind(item_i64)
            .bind(gid)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn genres_with_counts(&self) -> DomainResult<Vec<GenreCount>> {
        let rows = sqlx::query_as::<_, (i32, String, String, i64)>(
            "SELECT g.id, g.name, g.wire_id, COUNT(ig.item_id) \
             FROM genres g LEFT JOIN item_genres ig ON ig.genre_id = g.id \
             GROUP BY g.id, g.name, g.wire_id ORDER BY g.name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, wire_id, count)| GenreCount {
                genre: Genre {
                    id: id as i64,
                    name,
                    wire_id,
                },
                item_count: count.max(0) as u32,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_genre(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT ig.item_id FROM item_genres ig \
             JOIN genres g ON g.id = ig.genre_id \
             WHERE g.wire_id = $1 ORDER BY ig.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn backfill_genres(&self) -> DomainResult<u64> {
        let rows = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT id, genre FROM media_items WHERE genre IS NOT NULL AND genre <> ''",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        for (id, genre) in rows {
            let Some(raw) = genre else { continue };
            let names = pharos_core::split_genre_field(&raw);
            if names.is_empty() {
                continue;
            }
            let mid = id as MediaId;
            self.link_item_genres(mid, &names).await?;
        }
        let (total,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM item_genres")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(total.max(0) as u64)
    }
}

impl PersonStore for PostgresStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_person(
        &self,
        name: &str,
        sort_name: Option<&str>,
        provider_ids: Option<&str>,
        thumb_url: Option<&str>,
    ) -> DomainResult<i64> {
        let wire_id = pharos_core::person_wire_id(name);
        sqlx::query(
            "INSERT INTO people (name, sort_name, wire_id, provider_ids, thumb_url) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT(name) DO UPDATE SET \
                 sort_name    = COALESCE(excluded.sort_name, people.sort_name), \
                 provider_ids = COALESCE(excluded.provider_ids, people.provider_ids), \
                 thumb_url    = COALESCE(excluded.thumb_url, people.thumb_url)",
        )
        .bind(name)
        .bind(sort_name)
        .bind(&wire_id)
        .bind(provider_ids)
        .bind(thumb_url)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        // people.id is INTEGER (i32) on postgres; widen to i64 for the
        // core signature (the join column item_id is BIGINT).
        let (id,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM people WHERE name = $1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id as i64)
    }

    #[tracing::instrument(skip(self, people), fields(media.id = %item))]
    async fn link_item_people(&self, item: MediaId, people: &[PersonRef]) -> DomainResult<()> {
        let item_i64 = media_id_i64(item)?;
        let mut wanted: Vec<&PersonRef> = Vec::new();
        for p in people {
            if p.name.trim().is_empty() {
                continue;
            }
            let role = p.role.as_deref().unwrap_or("").trim();
            let dup = wanted.iter().any(|w| {
                w.name.trim() == p.name.trim() && w.role.as_deref().unwrap_or("").trim() == role
            });
            if !dup {
                wanted.push(p);
            }
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        sqlx::query("DELETE FROM item_people WHERE item_id = $1")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for p in &wanted {
            let name = p.name.trim();
            let wire_id = pharos_core::person_wire_id(name);
            sqlx::query(
                "INSERT INTO people (name, sort_name, wire_id, provider_ids, thumb_url) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT(name) DO UPDATE SET \
                     provider_ids = COALESCE(excluded.provider_ids, people.provider_ids), \
                     thumb_url    = COALESCE(excluded.thumb_url, people.thumb_url)",
            )
            .bind(name)
            .bind(Option::<&str>::None)
            .bind(&wire_id)
            .bind(p.provider_ids.as_deref())
            .bind(p.thumb.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (pid,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM people WHERE name = $1")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let role = p.role.as_deref().unwrap_or("").trim();
            let sort_order = p.sort_order.map(|n| n as i32);
            sqlx::query(
                "INSERT INTO item_people \
                     (item_id, person_id, role, character, person_kind, sort_order) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT(item_id, person_id, role) DO NOTHING",
            )
            .bind(item_i64)
            .bind(pid)
            .bind(role)
            .bind(p.character.as_deref())
            .bind(p.kind.as_str())
            .bind(sort_order)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn people_with_counts(&self) -> DomainResult<Vec<PersonCount>> {
        let rows = sqlx::query_as::<
            _,
            (
                i32,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<String>,
                i64,
            ),
        >(
            "SELECT p.id, p.name, p.sort_name, p.wire_id, p.provider_ids, p.thumb_url, \
                    COUNT(ip.item_id) \
             FROM people p LEFT JOIN item_people ip ON ip.person_id = p.id \
             GROUP BY p.id, p.name, p.sort_name, p.wire_id, p.provider_ids, p.thumb_url \
             ORDER BY COALESCE(p.sort_name, p.name)",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(
                |(id, name, sort_name, wire_id, provider_ids, thumb_url, count)| PersonCount {
                    person: Person {
                        id: id as i64,
                        name,
                        sort_name,
                        wire_id,
                        provider_ids,
                        thumb_url,
                    },
                    item_count: count.max(0) as u32,
                },
            )
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn person_by_wire_id(&self, wire_id: &str) -> DomainResult<Option<Person>> {
        let row = sqlx::query_as::<
            _,
            (
                i32,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<String>,
            ),
        >(
            "SELECT id, name, sort_name, wire_id, provider_ids, thumb_url \
             FROM people WHERE wire_id = $1 LIMIT 1",
        )
        .bind(wire_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(id, name, sort_name, wire_id, provider_ids, thumb_url)| Person {
                id: id as i64,
                name,
                sort_name,
                wire_id,
                provider_ids,
                thumb_url,
            },
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_person(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT DISTINCT ip.item_id FROM item_people ip \
             JOIN people p ON p.id = ip.person_id \
             WHERE p.wire_id = $1 ORDER BY ip.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn people_for_item(&self, item: MediaId) -> DomainResult<Vec<ItemPerson>> {
        let item_i64 = media_id_i64(item)?;
        let rows =
            sqlx::query_as::<_, (String, String, String, Option<String>, String, Option<i32>)>(
                "SELECT p.name, p.wire_id, ip.role, ip.character, ip.person_kind, ip.sort_order \
             FROM item_people ip JOIN people p ON p.id = ip.person_id \
             WHERE ip.item_id = $1 \
             ORDER BY ip.sort_order IS NULL, ip.sort_order, p.name",
            )
            .bind(item_i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(
                |(name, wire_id, role, character, kind, sort_order)| ItemPerson {
                    name,
                    wire_id,
                    role: Some(role).filter(|r| !r.is_empty()),
                    character,
                    kind: PersonKind::parse(&kind),
                    sort_order: sort_order.map(|n| n.max(0) as u32),
                },
            )
            .collect())
    }
}

impl StudioStore for PostgresStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_studio(&self, name: &str) -> DomainResult<i64> {
        let wire_id = pharos_core::studio_wire_id(name);
        sqlx::query(
            "INSERT INTO studios (name, wire_id) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
        )
        .bind(name)
        .bind(&wire_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        // studios.id is INTEGER (i32) on postgres; widen to i64 for the
        // core signature (the join column item_id is BIGINT).
        let (id,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM studios WHERE name = $1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id as i64)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_studios(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 = media_id_i64(item)?;
        let mut wanted: Vec<String> = Vec::new();
        for n in names {
            let t = n.trim();
            if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                wanted.push(t.to_string());
            }
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        sqlx::query("DELETE FROM item_studios WHERE item_id = $1")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for name in &wanted {
            let wire_id = pharos_core::studio_wire_id(name);
            sqlx::query(
                "INSERT INTO studios (name, wire_id) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (sid,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM studios WHERE name = $1")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO item_studios (item_id, studio_id) VALUES ($1, $2) \
                 ON CONFLICT(item_id, studio_id) DO NOTHING",
            )
            .bind(item_i64)
            .bind(sid)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn studios_with_counts(&self) -> DomainResult<Vec<StudioCount>> {
        let rows = sqlx::query_as::<_, (i32, String, String, i64)>(
            "SELECT s.id, s.name, s.wire_id, COUNT(is_.item_id) \
             FROM studios s LEFT JOIN item_studios is_ ON is_.studio_id = s.id \
             GROUP BY s.id, s.name, s.wire_id ORDER BY s.name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, wire_id, count)| StudioCount {
                studio: Studio {
                    id: id as i64,
                    name,
                    wire_id,
                },
                item_count: count.max(0) as u32,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_studio(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT is_.item_id FROM item_studios is_ \
             JOIN studios s ON s.id = is_.studio_id \
             WHERE s.wire_id = $1 ORDER BY is_.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn studios_for_item(&self, item: MediaId) -> DomainResult<Vec<Studio>> {
        let item_i64 = media_id_i64(item)?;
        let rows = sqlx::query_as::<_, (i32, String, String)>(
            "SELECT s.id, s.name, s.wire_id FROM item_studios is_ \
             JOIN studios s ON s.id = is_.studio_id \
             WHERE is_.item_id = $1 ORDER BY s.name",
        )
        .bind(item_i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, wire_id)| Studio {
                id: id as i64,
                name,
                wire_id,
            })
            .collect())
    }
}

impl TagStore for PostgresStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_tag(&self, name: &str) -> DomainResult<i64> {
        let wire_id = pharos_core::tag_wire_id(name);
        sqlx::query(
            "INSERT INTO tags (name, wire_id) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
        )
        .bind(name)
        .bind(&wire_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        // tags.id is INTEGER (i32) on postgres; widen to i64 for the core
        // signature (the join column item_id is BIGINT).
        let (id,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM tags WHERE name = $1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id as i64)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 = media_id_i64(item)?;
        let mut wanted: Vec<String> = Vec::new();
        for n in names {
            let t = n.trim();
            if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                wanted.push(t.to_string());
            }
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        sqlx::query("DELETE FROM item_tags WHERE item_id = $1")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for name in &wanted {
            let wire_id = pharos_core::tag_wire_id(name);
            sqlx::query(
                "INSERT INTO tags (name, wire_id) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (tid,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM tags WHERE name = $1")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO item_tags (item_id, tag_id) VALUES ($1, $2) \
                 ON CONFLICT(item_id, tag_id) DO NOTHING",
            )
            .bind(item_i64)
            .bind(tid)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn add_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<u64> {
        let item_i64 = media_id_i64(item)?;
        let mut wanted: Vec<String> = Vec::new();
        for n in names {
            let t = n.trim();
            if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                wanted.push(t.to_string());
            }
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut added = 0u64;
        for name in &wanted {
            let wire_id = pharos_core::tag_wire_id(name);
            sqlx::query(
                "INSERT INTO tags (name, wire_id) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (tid,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM tags WHERE name = $1")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let res = sqlx::query(
                "INSERT INTO item_tags (item_id, tag_id) VALUES ($1, $2) \
                 ON CONFLICT(item_id, tag_id) DO NOTHING",
            )
            .bind(item_i64)
            .bind(tid)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            added += res.rows_affected();
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(added)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn remove_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<u64> {
        let item_i64 = media_id_i64(item)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut removed = 0u64;
        for name in names {
            let t = name.trim();
            if t.is_empty() {
                continue;
            }
            let res = sqlx::query(
                "DELETE FROM item_tags WHERE item_id = $1 \
                 AND tag_id = (SELECT id FROM tags WHERE name = $2)",
            )
            .bind(item_i64)
            .bind(t)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            removed += res.rows_affected();
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(removed)
    }

    #[tracing::instrument(skip(self))]
    async fn tags_with_counts(&self) -> DomainResult<Vec<TagCount>> {
        let rows = sqlx::query_as::<_, (i32, String, String, i64)>(
            "SELECT t.id, t.name, t.wire_id, COUNT(it.item_id) \
             FROM tags t LEFT JOIN item_tags it ON it.tag_id = t.id \
             GROUP BY t.id, t.name, t.wire_id ORDER BY t.name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, wire_id, count)| TagCount {
                tag: Tag {
                    id: id as i64,
                    name,
                    wire_id,
                },
                item_count: count.max(0) as u32,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_tag(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT it.item_id FROM item_tags it \
             JOIN tags t ON t.id = it.tag_id \
             WHERE t.wire_id = $1 ORDER BY it.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn tags_for_item(&self, item: MediaId) -> DomainResult<Vec<Tag>> {
        let item_i64 = media_id_i64(item)?;
        let rows = sqlx::query_as::<_, (i32, String, String)>(
            "SELECT t.id, t.name, t.wire_id FROM item_tags it \
             JOIN tags t ON t.id = it.tag_id \
             WHERE it.item_id = $1 ORDER BY t.name",
        )
        .bind(item_i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, wire_id)| Tag {
                id: id as i64,
                name,
                wire_id,
            })
            .collect())
    }
}

impl CollectionStore for PostgresStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_collection(
        &self,
        name: &str,
        kind: Option<&str>,
        overview: Option<&str>,
    ) -> DomainResult<i64> {
        let wire_id = pharos_core::collection_wire_id(name);
        sqlx::query(
            "INSERT INTO collections (name, wire_id, kind, overview) \
             VALUES ($1, $2, COALESCE($3, 'boxset'), $4) \
             ON CONFLICT(name) DO UPDATE SET \
                 kind     = COALESCE(excluded.kind, collections.kind), \
                 overview = COALESCE(excluded.overview, collections.overview)",
        )
        .bind(name)
        .bind(&wire_id)
        .bind(kind)
        .bind(overview)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        // collections.id is INTEGER (i32) on postgres; widen for the core
        // signature (the join column item_id is BIGINT).
        let (id,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM collections WHERE name = $1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id as i64)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_collections(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 = media_id_i64(item)?;
        let mut wanted: Vec<String> = Vec::new();
        for n in names {
            let t = n.trim();
            if !t.is_empty() && !wanted.iter().any(|w| w == t) {
                wanted.push(t.to_string());
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for name in &wanted {
            let wire_id = pharos_core::collection_wire_id(name);
            sqlx::query(
                "INSERT INTO collections (name, wire_id, kind) VALUES ($1, $2, 'boxset') \
                 ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (cid,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM collections WHERE name = $1")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (next_order,) = sqlx::query_as::<_, (i32,)>(
                "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM collection_items \
                 WHERE collection_id = $1",
            )
            .bind(cid)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO collection_items (collection_id, item_id, sort_order) \
                 VALUES ($1, $2, $3) ON CONFLICT(collection_id, item_id) DO NOTHING",
            )
            .bind(cid)
            .bind(item_i64)
            .bind(next_order)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn collections_with_counts(&self) -> DomainResult<Vec<CollectionCount>> {
        let rows = sqlx::query_as::<_, (i32, String, String, String, Option<String>, i64)>(
            "SELECT c.id, c.name, c.wire_id, c.kind, c.overview, COUNT(ci.item_id) \
             FROM collections c LEFT JOIN collection_items ci ON ci.collection_id = c.id \
             GROUP BY c.id, c.name, c.wire_id, c.kind, c.overview ORDER BY c.name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(
                |(id, name, wire_id, kind, overview, count)| CollectionCount {
                    collection: Collection {
                        id: id as i64,
                        name,
                        wire_id,
                        kind,
                        overview,
                    },
                    item_count: count.max(0) as u32,
                },
            )
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn collection_by_wire_id(&self, wire_id: &str) -> DomainResult<Option<Collection>> {
        let row = sqlx::query_as::<_, (i32, String, String, String, Option<String>)>(
            "SELECT id, name, wire_id, kind, overview FROM collections WHERE wire_id = $1 LIMIT 1",
        )
        .bind(wire_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(|(id, name, wire_id, kind, overview)| Collection {
            id: id as i64,
            name,
            wire_id,
            kind,
            overview,
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn collection_items(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT ci.item_id FROM collection_items ci \
             JOIN collections c ON c.id = ci.collection_id \
             WHERE c.wire_id = $1 ORDER BY ci.sort_order, ci.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self, item_ids))]
    async fn create_collection(
        &self,
        name: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Collection> {
        let wire_id = pharos_core::collection_wire_id(name);
        self.upsert_collection(name, None, None).await?;
        if !item_ids.is_empty() {
            self.add_collection_items(&wire_id, item_ids).await?;
        }
        self.collection_by_wire_id(&wire_id)
            .await?
            .ok_or_else(|| DomainError::Backend("collection vanished after create".into()))
    }

    #[tracing::instrument(skip(self, item_ids))]
    async fn add_collection_items(
        &self,
        wire_id: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Option<u64>> {
        let Some((cid,)) =
            sqlx::query_as::<_, (i32,)>("SELECT id FROM collections WHERE wire_id = $1")
                .bind(wire_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?
        else {
            return Ok(None);
        };
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let (mut next_order,) = sqlx::query_as::<_, (i32,)>(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM collection_items WHERE collection_id = $1",
        )
        .bind(cid)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut added = 0u64;
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for item in item_ids {
            let item_i64 = media_id_i64(*item)?;
            if !seen.insert(item_i64) {
                continue;
            }
            let res = sqlx::query(
                "INSERT INTO collection_items (collection_id, item_id, sort_order) \
                 VALUES ($1, $2, $3) ON CONFLICT(collection_id, item_id) DO NOTHING",
            )
            .bind(cid)
            .bind(item_i64)
            .bind(next_order)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            if res.rows_affected() > 0 {
                added += 1;
                next_order += 1;
            }
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(Some(added))
    }

    #[tracing::instrument(skip(self, item_ids))]
    async fn remove_collection_items(
        &self,
        wire_id: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Option<u64>> {
        let Some((cid,)) =
            sqlx::query_as::<_, (i32,)>("SELECT id FROM collections WHERE wire_id = $1")
                .bind(wire_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?
        else {
            return Ok(None);
        };
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut removed = 0u64;
        for item in item_ids {
            let item_i64 = media_id_i64(*item)?;
            let res = sqlx::query(
                "DELETE FROM collection_items WHERE collection_id = $1 AND item_id = $2",
            )
            .bind(cid)
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            removed += res.rows_affected();
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(Some(removed))
    }
}

impl PlaylistStore for PostgresStore {
    #[tracing::instrument(skip(self, item_ids))]
    async fn create_playlist(
        &self,
        name: &str,
        owner_user_id: Option<&str>,
        media_type: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Playlist> {
        let wire_id = uuid::Uuid::new_v4().simple().to_string();
        let now = now_unix_secs();
        sqlx::query(
            "INSERT INTO playlists (wire_id, name, owner_user_id, media_type, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&wire_id)
        .bind(name)
        .bind(owner_user_id)
        .bind(media_type)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        if !item_ids.is_empty() {
            self.add_playlist_items(&wire_id, item_ids).await?;
        }
        self.playlist_by_wire_id(&wire_id)
            .await?
            .ok_or_else(|| DomainError::Backend("playlist vanished after create".into()))
    }

    #[tracing::instrument(skip(self))]
    async fn playlist_by_wire_id(&self, wire_id: &str) -> DomainResult<Option<Playlist>> {
        let row = sqlx::query_as::<_, (i64, String, String, Option<String>, String)>(
            "SELECT id, wire_id, name, owner_user_id, media_type FROM playlists \
             WHERE wire_id = $1 LIMIT 1",
        )
        .bind(wire_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(
            row.map(|(id, wire_id, name, owner_user_id, media_type)| Playlist {
                id,
                wire_id,
                name,
                owner_user_id,
                media_type,
            }),
        )
    }

    #[tracing::instrument(skip(self))]
    async fn playlists_for_owner(
        &self,
        owner_user_id: Option<&str>,
    ) -> DomainResult<Vec<Playlist>> {
        let rows = sqlx::query_as::<_, (i64, String, String, Option<String>, String)>(
            "SELECT id, wire_id, name, owner_user_id, media_type FROM playlists \
             WHERE owner_user_id IS NULL OR owner_user_id = $1 ORDER BY name",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, wire_id, name, owner_user_id, media_type)| Playlist {
                id,
                wire_id,
                name,
                owner_user_id,
                media_type,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn playlist_entries(&self, wire_id: &str) -> DomainResult<Vec<PlaylistEntry>> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT pi.entry_id, pi.item_id FROM playlist_items pi \
             JOIN playlists p ON p.id = pi.playlist_id \
             WHERE p.wire_id = $1 ORDER BY pi.sort_order, pi.entry_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(entry_id, item_id)| PlaylistEntry {
                entry_id,
                item_id: item_id as MediaId,
            })
            .collect())
    }

    #[tracing::instrument(skip(self, item_ids))]
    async fn add_playlist_items(
        &self,
        wire_id: &str,
        item_ids: &[MediaId],
    ) -> DomainResult<Option<u64>> {
        let Some((pid,)) =
            sqlx::query_as::<_, (i64,)>("SELECT id FROM playlists WHERE wire_id = $1")
                .bind(wire_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?
        else {
            return Ok(None);
        };
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let (mut next_order,) = sqlx::query_as::<_, (i64,)>(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM playlist_items WHERE playlist_id = $1",
        )
        .bind(pid)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut added = 0u64;
        for item in item_ids {
            let item_i64 = i64::try_from(*item)
                .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
            let entry_id = uuid::Uuid::new_v4().simple().to_string();
            sqlx::query(
                "INSERT INTO playlist_items (playlist_id, entry_id, item_id, sort_order) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(pid)
            .bind(&entry_id)
            .bind(item_i64)
            .bind(next_order)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            added += 1;
            next_order += 1;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(Some(added))
    }

    #[tracing::instrument(skip(self, entry_ids))]
    async fn remove_playlist_entries(
        &self,
        wire_id: &str,
        entry_ids: &[String],
    ) -> DomainResult<Option<u64>> {
        let Some((pid,)) =
            sqlx::query_as::<_, (i64,)>("SELECT id FROM playlists WHERE wire_id = $1")
                .bind(wire_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?
        else {
            return Ok(None);
        };
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut removed = 0u64;
        for entry_id in entry_ids {
            let res =
                sqlx::query("DELETE FROM playlist_items WHERE playlist_id = $1 AND entry_id = $2")
                    .bind(pid)
                    .bind(entry_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| DomainError::Backend(e.to_string()))?;
            removed += res.rows_affected();
        }
        repack_playlist_order_pg(&mut tx, pid).await?;
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(Some(removed))
    }

    #[tracing::instrument(skip(self))]
    async fn move_playlist_entry(
        &self,
        wire_id: &str,
        entry_id: &str,
        new_index: usize,
    ) -> DomainResult<Option<bool>> {
        let Some((pid,)) =
            sqlx::query_as::<_, (i64,)>("SELECT id FROM playlists WHERE wire_id = $1")
                .bind(wire_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?
        else {
            return Ok(None);
        };
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let ordered: Vec<String> = sqlx::query_as::<_, (String,)>(
            "SELECT entry_id FROM playlist_items WHERE playlist_id = $1 ORDER BY sort_order, entry_id",
        )
        .bind(pid)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?
        .into_iter()
        .map(|(e,)| e)
        .collect();
        let Some(cur) = ordered.iter().position(|e| e == entry_id) else {
            return Ok(Some(false));
        };
        let mut reordered = ordered.clone();
        let moved = reordered.remove(cur);
        let target = new_index.min(reordered.len());
        reordered.insert(target, moved);
        for (idx, e) in reordered.iter().enumerate() {
            sqlx::query(
                "UPDATE playlist_items SET sort_order = $1 WHERE playlist_id = $2 AND entry_id = $3",
            )
            .bind(idx as i64)
            .bind(pid)
            .bind(e)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(Some(true))
    }

    #[tracing::instrument(skip(self))]
    async fn delete_playlist(&self, wire_id: &str) -> DomainResult<Option<()>> {
        let res = sqlx::query("DELETE FROM playlists WHERE wire_id = $1")
            .bind(wire_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        if res.rows_affected() == 0 {
            Ok(None)
        } else {
            Ok(Some(()))
        }
    }
}

/// Re-pack a playlist's `sort_order` to contiguous 0..n (postgres twin of
/// the sqlite `repack_playlist_order`).
async fn repack_playlist_order_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    playlist_id: i64,
) -> DomainResult<()> {
    let ordered: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT entry_id FROM playlist_items WHERE playlist_id = $1 ORDER BY sort_order, entry_id",
    )
    .bind(playlist_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| DomainError::Backend(e.to_string()))?
    .into_iter()
    .map(|(e,)| e)
    .collect();
    for (idx, e) in ordered.iter().enumerate() {
        sqlx::query(
            "UPDATE playlist_items SET sort_order = $1 WHERE playlist_id = $2 AND entry_id = $3",
        )
        .bind(idx as i64)
        .bind(playlist_id)
        .bind(e)
        .execute(&mut **tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
    }
    Ok(())
}

impl LibraryStore for PostgresStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_library(
        &self,
        name: &str,
        root_path: &str,
        kind: LibraryKind,
        wire_id: &str,
    ) -> DomainResult<i64> {
        let now = now_unix_secs();
        sqlx::query(
            "INSERT INTO libraries (name, root_path, kind, wire_id, created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT(root_path) DO UPDATE SET name = excluded.name, \
                kind = excluded.kind, wire_id = excluded.wire_id",
        )
        .bind(name)
        .bind(root_path)
        .bind(kind.collection_type())
        .bind(wire_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        // libraries.id is INTEGER (i32) on postgres; widen to i64.
        let (id,) = sqlx::query_as::<_, (i32,)>("SELECT id FROM libraries WHERE root_path = $1")
            .bind(root_path)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id as i64)
    }

    #[tracing::instrument(skip(self))]
    async fn delete_library(&self, root_path: &str) -> DomainResult<()> {
        sqlx::query("DELETE FROM libraries WHERE root_path = $1")
            .bind(root_path)
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn libraries(&self) -> DomainResult<Vec<Library>> {
        let rows = sqlx::query_as::<_, (i32, String, String, String, String)>(
            "SELECT id, name, root_path, kind, wire_id FROM libraries ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, root_path, kind, wire_id)| Library {
                id: id as i64,
                name,
                root_path,
                kind: LibraryKind::parse(&kind),
                wire_id,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn backfill_library_ids(&self) -> DomainResult<u64> {
        let libs = self.libraries().await?;
        for lib in &libs {
            let like = crate::root_like_pattern(&lib.root_path);
            sqlx::query("UPDATE media_items SET library_id = $1 WHERE path LIKE $2 ESCAPE '\\'")
                .bind(lib.id as i32)
                .bind(like)
                .execute(&self.pool)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
        }
        let (count,) = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM media_items WHERE library_id IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(count.max(0) as u64)
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_library(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT m.id FROM media_items m \
             JOIN libraries l ON l.id = m.library_id \
             WHERE l.wire_id = $1 ORDER BY m.id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }
}

impl UserStore for PostgresStore {
    #[tracing::instrument(skip(self, record), fields(user.name = %record.name))]
    async fn create(&self, record: UserRecord) -> AuthResult<()> {
        let id_bytes = record.id.0.as_bytes().to_vec();
        let admin: i32 = if record.policy.admin { 1 } else { 0 };
        let res = sqlx::query(
            "INSERT INTO users (id, name, password_hash, admin) VALUES ($1, $2, $3, $4)",
        )
        .bind(id_bytes)
        .bind(&record.name)
        .bind(record.password_hash.expose())
        .bind(admin)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.constraint().is_some() => Err(AuthError::Conflict),
            Err(e) => Err(map_sqlx(e)),
        }
    }

    #[tracing::instrument(skip(self), fields(user.name = %name))]
    async fn lookup_by_name(&self, name: &str) -> AuthResult<UserRecord> {
        let row: Option<(Vec<u8>, String, String, i32)> =
            sqlx::query_as("SELECT id, name, password_hash, admin FROM users WHERE name = $1")
                .bind(name)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx)?;
        row.map(record_from_row)
            .transpose()?
            .ok_or(AuthError::UserNotFound)
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn get(&self, id: UserId) -> AuthResult<UserRecord> {
        let id_bytes = id.0.as_bytes().to_vec();
        let row: Option<(Vec<u8>, String, String, i32)> =
            sqlx::query_as("SELECT id, name, password_hash, admin FROM users WHERE id = $1")
                .bind(id_bytes)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx)?;
        row.map(record_from_row)
            .transpose()?
            .ok_or(AuthError::UserNotFound)
    }

    #[tracing::instrument(skip(self))]
    async fn list(&self) -> AuthResult<Vec<UserRecord>> {
        let rows: Vec<(Vec<u8>, String, String, i32)> =
            sqlx::query_as("SELECT id, name, password_hash, admin FROM users ORDER BY LOWER(name)")
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?;
        rows.into_iter().map(record_from_row).collect()
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn delete(&self, id: UserId) -> AuthResult<()> {
        let id_bytes = id.0.as_bytes().to_vec();
        let res = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(id_bytes)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %id))]
    async fn set_policy(&self, id: UserId, policy: UserPolicy) -> AuthResult<()> {
        let id_bytes = id.0.as_bytes().to_vec();
        let admin: i32 = if policy.admin { 1 } else { 0 };
        let res = sqlx::query("UPDATE users SET admin = $1 WHERE id = $2")
            .bind(admin)
            .bind(id_bytes)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }

    async fn set_password(
        &self,
        id: UserId,
        password_hash: pharos_core::SecretString,
    ) -> AuthResult<()> {
        let id_bytes = id.0.as_bytes().to_vec();
        let res = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
            .bind(password_hash.expose())
            .bind(id_bytes)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(AuthError::UserNotFound);
        }
        Ok(())
    }
}

impl TokenStore for PostgresStore {
    #[tracing::instrument(skip(self), fields(user.id = %user_id, device = %device_id))]
    async fn issue(&self, user_id: UserId, device_id: &str) -> AuthResult<AuthToken> {
        // The raw token is returned to the client but never stored — only
        // its SHA-256 hash is persisted (a DB leak must not yield usable
        // tokens). `expires_at` bounds the token's lifetime.
        let token = Uuid::new_v4().simple().to_string();
        let now = now_unix_secs();
        let user_bytes = user_id.0.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO auth_tokens (token_hash, user_id, device_id, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(crate::auth_token::hash(&token))
        .bind(user_bytes)
        .bind(device_id)
        .bind(now)
        .bind(now + crate::auth_token::TTL_SECS)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(AuthToken(SecretString::new(token)))
    }

    #[tracing::instrument(skip(self, token))]
    async fn resolve(&self, token: &str) -> AuthResult<UserId> {
        // Look up by hash and reject expired tokens (expires_at NULL = never).
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT user_id FROM auth_tokens \
             WHERE token_hash = $1 AND (expires_at IS NULL OR expires_at > $2)",
        )
        .bind(crate::auth_token::hash(token))
        .bind(now_unix_secs())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let (bytes,) = row.ok_or(AuthError::InvalidToken)?;
        let uuid =
            Uuid::from_slice(&bytes).map_err(|e| AuthError::Backend(format!("bad uuid: {e}")))?;
        Ok(UserId(uuid))
    }

    #[tracing::instrument(skip(self, token))]
    async fn revoke(&self, token: &str) -> AuthResult<()> {
        sqlx::query("DELETE FROM auth_tokens WHERE token_hash = $1")
            .bind(crate::auth_token::hash(token))
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple()))]
    async fn tokens_for(&self, user: UserId) -> AuthResult<Vec<pharos_core::TokenRecord>> {
        let user_bytes = user.0.as_bytes().to_vec();
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT device_id, created_at FROM auth_tokens WHERE user_id = $1
             ORDER BY created_at DESC",
        )
        .bind(user_bytes)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(rows
            .into_iter()
            .map(
                |(device_id, issued_at_unix_secs)| pharos_core::TokenRecord {
                    device_id,
                    issued_at_unix_secs,
                },
            )
            .collect())
    }

    /// T58 phase 3 — revoke every token belonging to `user` whose
    /// `device_id` matches the supplied value (mirrors the sqlite twin).
    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple()))]
    async fn revoke_tokens_by_device(&self, user: UserId, device_id: &str) -> AuthResult<u64> {
        let user_bytes = user.0.as_bytes().to_vec();
        let res = sqlx::query("DELETE FROM auth_tokens WHERE user_id = $1 AND device_id = $2")
            .bind(user_bytes)
            .bind(device_id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(res.rows_affected())
    }
}

impl UserDataStore for PostgresStore {
    #[tracing::instrument(skip(self), fields(user.id = %user.0, media.id = %item))]
    async fn get_user_data(&self, user: UserId, item: MediaId) -> DomainResult<UserItemData> {
        let id_bytes = user.0.as_bytes().to_vec();
        let item_i64 = media_id_i64(item)?;
        let row: Option<(i32, i32, i64, i32, i64)> = sqlx::query_as(
            "SELECT played, play_count, last_played_position_ticks, is_favorite, last_played_at
             FROM user_data WHERE user_id = $1 AND item_id = $2",
        )
        .bind(id_bytes)
        .bind(item_i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(row_to_user_data).unwrap_or_default())
    }

    #[tracing::instrument(skip(self, data), fields(user.id = %user.0, media.id = %item))]
    async fn set_user_data(
        &self,
        user: UserId,
        item: MediaId,
        data: UserItemData,
    ) -> DomainResult<()> {
        let id_bytes = user.0.as_bytes().to_vec();
        let item_i64 = media_id_i64(item)?;
        let played: i32 = if data.played { 1 } else { 0 };
        let fav: i32 = if data.is_favorite { 1 } else { 0 };
        let pos_i64 = i64::try_from(data.last_played_position_ticks)
            .map_err(|e| DomainError::Backend(format!("position overflow: {e}")))?;
        let pc_i32 = i32::try_from(data.play_count)
            .map_err(|e| DomainError::Backend(format!("play_count overflow: {e}")))?;
        sqlx::query(
            "INSERT INTO user_data
               (user_id, item_id, played, play_count, last_played_position_ticks,
                is_favorite, last_played_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (user_id, item_id) DO UPDATE SET
               played = EXCLUDED.played,
               play_count = EXCLUDED.play_count,
               last_played_position_ticks = EXCLUDED.last_played_position_ticks,
               is_favorite = EXCLUDED.is_favorite,
               last_played_at = EXCLUDED.last_played_at",
        )
        .bind(id_bytes)
        .bind(item_i64)
        .bind(played)
        .bind(pc_i32)
        .bind(pos_i64)
        .bind(fav)
        .bind(data.last_played_at)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self, items), fields(user.id = %user.0, count = items.len()))]
    async fn user_data_bulk(
        &self,
        user: UserId,
        items: &[MediaId],
    ) -> DomainResult<Vec<UserItemData>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let id_bytes = user.0.as_bytes().to_vec();
        // Postgres supports `= ANY($2)` with an array binding —
        // cleaner than the sqlite per-?-placeholder trick.
        let ids_i64: Vec<i64> = items
            .iter()
            .map(|id| media_id_i64(*id))
            .collect::<DomainResult<_>>()?;
        let rows: Vec<(i64, i32, i32, i64, i32, i64)> = sqlx::query_as(
            "SELECT item_id, played, play_count, last_played_position_ticks,
                    is_favorite, last_played_at
             FROM user_data
             WHERE user_id = $1 AND item_id = ANY($2)",
        )
        .bind(id_bytes)
        .bind(&ids_i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut by_id: std::collections::HashMap<i64, UserItemData> =
            std::collections::HashMap::with_capacity(rows.len());
        for (id, played, pc, pos, fav, lp) in rows {
            by_id.insert(id, row_to_user_data((played, pc, pos, fav, lp)));
        }
        Ok(ids_i64
            .iter()
            .map(|id| by_id.get(id).copied().unwrap_or_default())
            .collect())
    }

    #[tracing::instrument(skip(self), fields(user.id = %user.0))]
    async fn resumable_items(&self, user: UserId) -> DomainResult<Vec<MediaId>> {
        let id_bytes = user.0.as_bytes().to_vec();
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT item_id FROM user_data
             WHERE user_id = $1 AND last_played_position_ticks > 0 AND played = 0
             ORDER BY last_played_at DESC",
        )
        .bind(id_bytes)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        rows.into_iter()
            .map(|(id,)| {
                u64::try_from(id).map_err(|e| DomainError::Backend(format!("id negative: {e}")))
            })
            .collect()
    }
}

/// LIB-B1 — page row: flattened [`MediaRow`] + the window count total.
#[derive(sqlx::FromRow)]
struct QueryRow {
    #[sqlx(flatten)]
    media: MediaRow,
    total_count: i64,
}

#[derive(sqlx::FromRow)]
struct MediaRow {
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
    width: Option<i32>,
    height: Option<i32>,
    frame_rate_mille: Option<i32>,
    audio_channels: Option<i32>,
    sample_rate: Option<i32>,
    series_name: Option<String>,
    season_number: Option<i32>,
    episode_number: Option<i32>,
    subtitle_tracks_json: Option<String>,
    attachments_json: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    created_at: Option<i64>,
    chapters_json: Option<String>,
    video_profile: Option<String>,
    video_level: Option<i32>,
    pixel_format: Option<String>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    audio_tracks_json: Option<String>,
    community_rating: Option<f32>,
    critic_rating: Option<f32>,
    official_rating: Option<String>,
    production_year: Option<i32>,
    premiere_date: Option<i64>,
    overview: Option<String>,
    tagline: Option<String>,
    provider_ids: Option<String>,
    production_locations_json: Option<String>,
    trailers_json: Option<String>,
    series_folder: Option<String>,
    series_year: Option<i32>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    release_year: Option<i32>,
}

impl MediaRow {
    fn into_domain(self) -> DomainResult<MediaItem> {
        let id = u64::try_from(self.id)
            .map_err(|e| DomainError::Backend(format!("id negative: {e}")))?;
        let kind = MediaKind::from_str(&self.kind)?;
        let probe = MediaProbe {
            size_bytes: self.size_bytes.and_then(|v| u64::try_from(v).ok()),
            duration_ms: self.duration_ms.and_then(|v| u64::try_from(v).ok()),
            container: self.container,
            bitrate_bps: self.bitrate_bps.and_then(|v| u64::try_from(v).ok()),
            video_codec: self.video_codec,
            video_profile: self.video_profile,
            video_level: self.video_level.and_then(|v| u32::try_from(v).ok()),
            pixel_format: self.pixel_format,
            color_primaries: self.color_primaries,
            color_transfer: self.color_transfer,
            color_space: self.color_space,
            audio_codec: self.audio_codec,
            width: self.width.and_then(|v| u32::try_from(v).ok()),
            height: self.height.and_then(|v| u32::try_from(v).ok()),
            frame_rate_mille: self.frame_rate_mille.and_then(|v| u32::try_from(v).ok()),
            audio_channels: self.audio_channels.and_then(|v| u32::try_from(v).ok()),
            sample_rate: self.sample_rate.and_then(|v| u32::try_from(v).ok()),
            subtitle_tracks: crate::subtitle_track_json::decode(
                self.subtitle_tracks_json.as_deref(),
            ),
            audio_tracks: crate::audio_track_json::decode(self.audio_tracks_json.as_deref()),
            attachments: crate::attachment_json::decode(self.attachments_json.as_deref()),
            // Scan-transient: the embedded track title is folded into the
            // `media_items.title` column, not stored on the probe. See the
            // sqlite store's `into_domain` for the rationale.
            title: None,
            artist: self.artist,
            album: self.album,
            album_artist: self.album_artist,
            genre: self.genre,
            track_number: self.track_number.and_then(|v| u32::try_from(v).ok()),
            disc_number: self.disc_number.and_then(|v| u32::try_from(v).ok()),
            year: self.release_year.and_then(|v| u32::try_from(v).ok()),
            chapters: crate::chapter_json::decode(self.chapters_json.as_deref()),
            // P34 — alternate editions enrichment lives in the
            // scanner; postgres rows today never carry them.
            alternate_sources: Vec::new(),
        };
        let series = self.series_name.map(|name| SeriesInfo {
            series_name: name,
            season_number: self.season_number.and_then(|v| u32::try_from(v).ok()),
            episode_number: self.episode_number.and_then(|v| u32::try_from(v).ok()),
            series_folder: self.series_folder,
            series_year: self.series_year.and_then(|v| u32::try_from(v).ok()),
        });
        let metadata = MediaMetadata {
            community_rating: self.community_rating,
            critic_rating: self.critic_rating,
            official_rating: self.official_rating,
            production_year: self.production_year.and_then(|v| u32::try_from(v).ok()),
            premiere_date: self.premiere_date,
            overview: self.overview,
            tagline: self.tagline,
            provider_ids: crate::provider_ids_json::decode(self.provider_ids.as_deref()),
            production_locations: crate::string_list_json::decode(
                self.production_locations_json.as_deref(),
            ),
            trailers: crate::string_list_json::decode(self.trailers_json.as_deref()),
        };
        Ok(MediaItem {
            id,
            path: self.path.into(),
            title: self.title,
            kind,
            probe,
            series,
            created_at: self.created_at,
            metadata,
        })
    }
}

impl PreferenceStore for PostgresStore {
    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple()))]
    async fn get_user_configuration(&self, user: UserId) -> DomainResult<Option<String>> {
        let id_bytes = user.0.as_bytes().to_vec();
        let row: Option<(String,)> =
            sqlx::query_as("SELECT config FROM user_configuration WHERE user_id = $1")
                .bind(id_bytes)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(|(s,)| s))
    }

    #[tracing::instrument(skip(self, json), fields(user.id = %user.0.simple(), bytes = json.len()))]
    async fn set_user_configuration(&self, user: UserId, json: &str) -> DomainResult<()> {
        let id_bytes = user.0.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO user_configuration (user_id, config, updated_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (user_id) DO UPDATE SET config = EXCLUDED.config,
                                                 updated_at = EXCLUDED.updated_at",
        )
        .bind(id_bytes)
        .bind(json)
        .bind(now_unix_secs())
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user.id = %user.0.simple(), dp_id = %dp_id, client = %client))]
    async fn get_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
    ) -> DomainResult<Option<String>> {
        let id_bytes = user.0.as_bytes().to_vec();
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT prefs FROM display_preferences \
             WHERE user_id = $1 AND dp_id = $2 AND client = $3",
        )
        .bind(id_bytes)
        .bind(dp_id)
        .bind(client)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(|(s,)| s))
    }

    #[tracing::instrument(skip(self, json), fields(user.id = %user.0.simple(), dp_id = %dp_id, client = %client))]
    async fn set_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
        json: &str,
    ) -> DomainResult<()> {
        let id_bytes = user.0.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO display_preferences (user_id, dp_id, client, prefs, updated_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (user_id, dp_id, client) DO UPDATE SET prefs = EXCLUDED.prefs,
                                                                updated_at = EXCLUDED.updated_at",
        )
        .bind(id_bytes)
        .bind(dp_id)
        .bind(client)
        .bind(json)
        .bind(now_unix_secs())
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }
}

impl TranscodeSessionStore for PostgresStore {
    #[tracing::instrument(skip(self, session), fields(psid = %play_session_id))]
    async fn upsert_transcode_session(
        &self,
        play_session_id: &str,
        session: &PersistedTranscodeSession,
        now_unix_secs: i64,
    ) -> DomainResult<()> {
        let media_id = media_id_i64(session.media_id)?;
        sqlx::query(
            "INSERT INTO transcode_sessions \
             (play_session_id, media_id, decision_json, source_probe_json, updated_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (play_session_id) DO UPDATE SET \
               media_id = EXCLUDED.media_id, \
               decision_json = EXCLUDED.decision_json, \
               source_probe_json = EXCLUDED.source_probe_json, \
               updated_at = EXCLUDED.updated_at",
        )
        .bind(play_session_id)
        .bind(media_id)
        .bind(&session.decision_json)
        .bind(&session.source_probe_json)
        .bind(now_unix_secs)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(psid = %play_session_id))]
    async fn get_transcode_session(
        &self,
        play_session_id: &str,
    ) -> DomainResult<Option<PersistedTranscodeSession>> {
        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT media_id, decision_json, source_probe_json \
             FROM transcode_sessions WHERE play_session_id = $1",
        )
        .bind(play_session_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(media_id, decision_json, source_probe_json)| PersistedTranscodeSession {
                media_id: media_id as u64,
                decision_json,
                source_probe_json,
            },
        ))
    }

    #[tracing::instrument(skip(self), fields(psid = %play_session_id))]
    async fn remove_transcode_session(&self, play_session_id: &str) -> DomainResult<()> {
        sqlx::query("DELETE FROM transcode_sessions WHERE play_session_id = $1")
            .bind(play_session_id)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn prune_transcode_sessions(&self, cutoff_unix_secs: i64) -> DomainResult<u64> {
        let res = sqlx::query("DELETE FROM transcode_sessions WHERE updated_at < $1")
            .bind(cutoff_unix_secs)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(res.rows_affected())
    }
}

impl SyncGroupStore for PostgresStore {
    #[tracing::instrument(skip(self, group), fields(group_id = %group.group_id))]
    async fn upsert_sync_group(
        &self,
        group: &PersistedSyncGroup,
        now_unix_secs: i64,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO sync_groups \
             (group_id, epoch_unix_ms, state_json, updated_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (group_id) DO UPDATE SET \
               epoch_unix_ms = EXCLUDED.epoch_unix_ms, \
               state_json = EXCLUDED.state_json, \
               updated_at = EXCLUDED.updated_at",
        )
        .bind(&group.group_id)
        .bind(group.epoch_unix_ms)
        .bind(&group.state_json)
        .bind(now_unix_secs)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id))]
    async fn get_sync_group(&self, group_id: &str) -> DomainResult<Option<PersistedSyncGroup>> {
        let row: Option<(i64, String, i64)> = sqlx::query_as(
            "SELECT epoch_unix_ms, state_json, updated_at \
             FROM sync_groups WHERE group_id = $1",
        )
        .bind(group_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(epoch_unix_ms, state_json, updated_at)| PersistedSyncGroup {
                group_id: group_id.to_string(),
                epoch_unix_ms,
                state_json,
                updated_at,
            },
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn list_sync_groups(&self) -> DomainResult<Vec<PersistedSyncGroup>> {
        let rows: Vec<(String, i64, String, i64)> = sqlx::query_as(
            "SELECT group_id, epoch_unix_ms, state_json, updated_at FROM sync_groups",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(
                |(group_id, epoch_unix_ms, state_json, updated_at)| PersistedSyncGroup {
                    group_id,
                    epoch_unix_ms,
                    state_json,
                    updated_at,
                },
            )
            .collect())
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id))]
    async fn remove_sync_group(&self, group_id: &str) -> DomainResult<()> {
        sqlx::query("DELETE FROM sync_groups WHERE group_id = $1")
            .bind(group_id)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn prune_sync_groups(&self, cutoff_unix_secs: i64) -> DomainResult<u64> {
        let res = sqlx::query("DELETE FROM sync_groups WHERE updated_at < $1")
            .bind(cutoff_unix_secs)
            .execute(self.pool())
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(res.rows_affected())
    }
}

fn record_from_row(row: (Vec<u8>, String, String, i32)) -> AuthResult<UserRecord> {
    let uuid =
        Uuid::from_slice(&row.0).map_err(|e| AuthError::Backend(format!("bad uuid: {e}")))?;
    Ok(UserRecord {
        id: UserId(uuid),
        name: row.1,
        password_hash: SecretString::new(row.2),
        policy: UserPolicy { admin: row.3 != 0 },
    })
}

fn row_to_user_data(row: (i32, i32, i64, i32, i64)) -> UserItemData {
    let (played, pc, pos, fav, lp) = row;
    UserItemData {
        played: played != 0,
        play_count: u32::try_from(pc).unwrap_or(0),
        last_played_position_ticks: u64::try_from(pos).unwrap_or(0),
        is_favorite: fav != 0,
        last_played_at: lp,
    }
}
