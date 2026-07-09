use crate::StoreError;
use pharos_core::{
    Collection, CollectionCount, CollectionStore, DomainError, DomainResult, Fingerprint, Genre,
    GenreCount, GenreStore, ItemPerson, Library, LibraryKind, LibraryStore, MediaId, MediaItem,
    MediaKind, MediaMetadata, MediaProbe, MediaQuery, MediaStore, Person, PersonCount, PersonKind,
    PersonRef, PersonStore, ScanState, SeriesInfo, Studio, StudioCount, StudioStore, Tag, TagCount,
    TagStore, UserId,
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    SqlitePool,
};
use std::str::FromStr;

const MEDIA_COLUMNS: &str = "id, path, title, kind, size_bytes, duration_ms, container, \
    bitrate_bps, video_codec, audio_codec, width, height, frame_rate_mille, \
    audio_channels, sample_rate, series_name, season_number, episode_number, \
    subtitle_tracks_json, attachments_json, artist, album, album_artist, genre, created_at, chapters_json, \
    video_profile, video_level, pixel_format, color_primaries, color_transfer, color_space, \
    audio_tracks_json, community_rating, critic_rating, official_rating, production_year, \
    premiere_date, overview, tagline, provider_ids, series_folder, series_year";

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

/// LIB-B4 — `MEDIA_COLUMNS` with each column qualified by a table alias,
/// for the search join (`SELECT m.id, m.path, …` so the FromRow still maps
/// `MediaRow`'s plain column names).
fn media_columns_prefixed(alias: &str) -> String {
    MEDIA_COLUMNS
        .split(',')
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| format!("{alias}.{c} AS {c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open a pool against the given sqlx connect URL (e.g. `sqlite::memory:`,
    /// `sqlite:///var/lib/pharos/data.db`). Runs migrations to latest.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        // WAL + busy_timeout for the file-backed (on-PVC) deployment: the
        // server runs the periodic library rescan concurrently with read
        // traffic, so the journal must let readers proceed against a live
        // writer instead of serialising on the rollback journal's whole-db
        // lock. busy_timeout(15s) absorbs the brief writer-lock windows the
        // scan's sequential writes take (V10 keeps writes single-threaded,
        // but a slow PVC can still stall a lock past the default 5s).
        // NORMAL synchronous is the WAL-recommended durability/speed
        // trade-off. All three no-op for `sqlite::memory:`.
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(StoreError::Sqlx)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(15));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// LIB-B1 — total matching rows for `q` BEFORE LIMIT/OFFSET, used as
    /// the empty-page fallback for `query()` (an over-offset or
    /// matches-nothing page yields no window-count row). Shares the same
    /// WHERE / join builder so the count matches the page filters exactly.
    async fn query_count(&self, q: &pharos_core::MediaQuery) -> DomainResult<u64> {
        use crate::media_query::{self, Param};
        // Build with an unpaged clone so no LIMIT/OFFSET params land in the
        // count statement (the COUNT ignores ordering / paging).
        let mut counting = q.clone();
        counting.limit = None;
        counting.start_index = 0;
        counting.sort = Vec::new();
        let built = media_query::build(&counting, |_| "?".to_string(), "-1");
        let user = media_query::user_data_user(&counting);
        let join = if media_query::needs_user_data_join(&counting) {
            "LEFT JOIN user_data ud ON ud.item_id = media_items.id AND ud.user_id = ?"
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

    /// LIB-B4 — the FTS-`MATCH` ∪ substring `hit(rid, score)` subquery
    /// shared by [`SqliteStore::search_page`] + [`SqliteStore::search_total`].
    /// `score` is the bm25 rank for FTS hits (lower = better) and a large
    /// sentinel for substring-only hits so the FTS hits sort first. The
    /// `MIN(score)` GROUP collapses a row that matched both arms to its best
    /// (FTS) score. Param order: match_expr, then one needle per LIKE column
    /// (title, overview, series_name, artist, album).
    fn search_hit_subquery() -> &'static str {
        "SELECT rid, MIN(score) AS score FROM ( \
             SELECT f.rowid AS rid, bm25(media_fts) AS score \
             FROM media_fts f WHERE media_fts MATCH ? \
             UNION ALL \
             SELECT m2.id AS rid, 1e18 AS score FROM media_items m2 \
             WHERE (COALESCE(m2.title_fold, LOWER(m2.title)) LIKE ? ESCAPE '\\' \
                    OR LOWER(COALESCE(m2.overview, '')) LIKE ? ESCAPE '\\' \
                    OR LOWER(COALESCE(m2.series_name, '')) LIKE ? ESCAPE '\\' \
                    OR LOWER(COALESCE(m2.artist, '')) LIKE ? ESCAPE '\\' \
                    OR LOWER(COALESCE(m2.album, '')) LIKE ? ESCAPE '\\') \
         ) GROUP BY rid"
    }

    /// LIB-B4 — one ranked page of search hits.
    async fn search_page(
        &self,
        match_expr: &str,
        needle: &str,
        kinds: &[MediaKind],
        limit: i64,
        offset: i64,
    ) -> DomainResult<Vec<MediaItem>> {
        let kind_clause = if kinds.is_empty() {
            String::new()
        } else {
            let holes = vec!["?"; kinds.len()].join(", ");
            format!("AND m.kind IN ({holes})")
        };
        let sql = format!(
            "SELECT {cols} FROM ({hit}) hit \
             JOIN media_items m ON m.id = hit.rid \
             WHERE 1 = 1 {kind_clause} \
             ORDER BY hit.score ASC, m.id ASC LIMIT ? OFFSET ?",
            cols = media_columns_prefixed("m"),
            hit = Self::search_hit_subquery(),
        );
        let mut query = sqlx::query_as::<_, MediaRow>(&sql)
            .bind(match_expr)
            .bind(needle)
            .bind(needle)
            .bind(needle)
            .bind(needle)
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

    /// LIB-B4 — total distinct search hits BEFORE limit/offset.
    async fn search_total(
        &self,
        match_expr: &str,
        needle: &str,
        kinds: &[MediaKind],
    ) -> DomainResult<u64> {
        let kind_clause = if kinds.is_empty() {
            String::new()
        } else {
            let holes = vec!["?"; kinds.len()].join(", ");
            format!("AND m.kind IN ({holes})")
        };
        let sql = format!(
            "SELECT COUNT(*) FROM ({hit}) hit \
             JOIN media_items m ON m.id = hit.rid \
             WHERE 1 = 1 {kind_clause}",
            hit = Self::search_hit_subquery(),
        );
        let mut query = sqlx::query_as::<_, (i64,)>(&sql)
            .bind(match_expr)
            .bind(needle)
            .bind(needle)
            .bind(needle)
            .bind(needle)
            .bind(needle);
        for k in kinds {
            query = query.bind(k.as_str());
        }
        let (count,) = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(u64::try_from(count.max(0)).unwrap_or(0))
    }

    /// LIB-B5 — aggregate facet counts over the base query's WHERE scope.
    /// Builds the base WHERE/JOIN once (sharing the LIB-B1 builder) into a
    /// `matched(id)` subquery, then one GROUP BY per requested dimension.
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
        // Reuse the LIB-B1 builder for the base scope. Drop sort/paging —
        // facets describe the whole matched set, not a page.
        let mut scope = base.clone();
        scope.sort = Vec::new();
        scope.limit = None;
        scope.start_index = 0;
        let built = media_query::build(&scope, |_| "?".to_string(), "-1");
        let user = media_query::user_data_user(&scope);
        let join = if media_query::needs_user_data_join(&scope) {
            "LEFT JOIN user_data ud ON ud.item_id = media_items.id AND ud.user_id = ?"
        } else {
            ""
        };
        let where_clause = if built.where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", built.where_sql)
        };
        // `matched` = the ids in the base scope. Each facet GROUP BYs the
        // join rows of items in `matched`.
        let matched = format!("SELECT media_items.id FROM media_items {join} {where_clause}");

        // One facet statement: binds the base-scope params (user id first
        // when the user-data join is active, then every builder param) and
        // collapses the (value, wire_id, count) rows into FacetValues.
        async fn run_facet(
            pool: &SqlitePool,
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

        // Entity facets (genre / studio / tag / person): join the entity +
        // its item_<entity> table, restricted to the matched ids.
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

        // Scalar facets: production_year + official_rating, read straight
        // off media_items within the matched scope. wire_id == value (the
        // chip sends the raw value).
        if req.years {
            let sql = format!(
                "SELECT CAST(production_year AS TEXT), CAST(production_year AS TEXT), COUNT(*) AS c \
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

    /// Read or initialise this server's stable identity UUID. First call
    /// in a fresh install writes a new row; subsequent calls return the
    /// same value. Clients see the same `server_id` across pharos
    /// restarts so they don't have to re-pair (T35).
    pub async fn load_or_create_server_id(&self) -> Result<String, StoreError> {
        if let Some((id,)) =
            sqlx::query_as::<_, (String,)>("SELECT server_id FROM system_identity WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?
        {
            return Ok(id);
        }
        let new_id = uuid::Uuid::new_v4().simple().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Race-safe insert: if another process beat us to it, fall back
        // to its value.
        match sqlx::query(
            "INSERT INTO system_identity (id, server_id, created_at) VALUES (1, ?, ?)",
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
}

impl SqliteStore {
    /// Read the persisted runtime config snapshot (`runtime_config`
    /// row id=1). Returns `Default` when the row has never been
    /// written.
    pub async fn load_runtime_config(&self) -> Result<crate::RuntimeConfig, StoreError> {
        let row = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            "SELECT server_name, login_disclaimer, custom_css \
             FROM runtime_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((server_name, login_disclaimer, custom_css)) => crate::RuntimeConfig {
                server_name,
                login_disclaimer,
                custom_css,
            },
            None => crate::RuntimeConfig::default(),
        })
    }

    /// Upsert the runtime config snapshot. Callers pass a fully-formed
    /// `RuntimeConfig`; previous values are replaced wholesale (the
    /// dashboard always submits the full form).
    pub async fn set_runtime_config(&self, cfg: &crate::RuntimeConfig) -> Result<(), StoreError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        sqlx::query(
            "INSERT INTO runtime_config (id, server_name, login_disclaimer, custom_css, updated_at)
             VALUES (1, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 server_name = excluded.server_name,
                 login_disclaimer = excluded.login_disclaimer,
                 custom_css = excluded.custom_css,
                 updated_at = excluded.updated_at",
        )
        .bind(&cfg.server_name)
        .bind(&cfg.login_disclaimer)
        .bind(&cfg.custom_css)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// LIB-B2 — distinct `(series_folder, series_name)` keys, for resolving a
    /// `?ParentId=<series synth id>` to its folder/name without loading every
    /// item. The API hashes each candidate via `series_id_for_key` to find
    /// the match (the synth id is a one-way hash, so the components must be
    /// recovered from the column values). Episodes only (the only rows with a
    /// series). Cheap: distinct over an indexed-ish small column set.
    pub async fn distinct_series_keys(&self) -> Result<Vec<(Option<String>, String)>, StoreError> {
        let rows = sqlx::query_as::<_, (Option<String>, String)>(
            "SELECT DISTINCT series_folder, series_name FROM media_items \
             WHERE series_name IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// LIB-B2 — distinct `(series_folder, series_name, season_number)` keys,
    /// for resolving a `?ParentId=<season synth id>`.
    pub async fn distinct_season_keys(
        &self,
    ) -> Result<Vec<(Option<String>, String, i64)>, StoreError> {
        let rows = sqlx::query_as::<_, (Option<String>, String, i64)>(
            "SELECT DISTINCT series_folder, series_name, season_number FROM media_items \
             WHERE series_name IS NOT NULL AND season_number IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// LIB-B2 — distinct non-empty artist + album_artist names, for resolving
    /// a `?ParentId=<artist synth id>`. Union of both probe columns (the
    /// in-memory parent pivot matched either).
    pub async fn distinct_artist_names(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT artist AS n FROM media_items WHERE artist IS NOT NULL AND artist <> '' \
             UNION SELECT DISTINCT album_artist FROM media_items \
             WHERE album_artist IS NOT NULL AND album_artist <> ''",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    /// LIB-B2 — distinct non-empty album names, for resolving a
    /// `?ParentId=<album synth id>`.
    pub async fn distinct_album_names(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT album FROM media_items WHERE album IS NOT NULL AND album <> ''",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    /// LIB-B2 — distinct raw `genre` probe strings (each may itself be a
    /// `|`/`,`-joined list). The API splits + hashes these to resolve a
    /// `?ParentId=<genre synth id>` against the LEGACY probe column when the
    /// `item_genres` entity join is empty (rows scanned before LIB-C4 and not
    /// yet backfilled) — preserving the legacy in-memory fallback.
    pub async fn distinct_genre_fields(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT genre FROM media_items WHERE genre IS NOT NULL AND genre <> ''",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }
}

impl MediaStore for SqliteStore {
    #[tracing::instrument(skip(self), fields(media.id = %id))]
    async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
        let id_i64 =
            i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let sql = format!("SELECT {MEDIA_COLUMNS} FROM media_items WHERE id = ?");
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
        let id_i64 = i64::try_from(item.id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
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
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Prefer the caller-supplied created_at (round-tripping after
        // a get → put), fall back to "now" on first insert.
        let created_at = item.created_at.unwrap_or(now_secs);
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
                audio_tracks_json, attachments_json, \
                community_rating, critic_rating, official_rating, production_year, \
                premiere_date, overview, tagline, provider_ids, \
                series_folder, series_year, title_fold) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                     ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET path = excluded.path,
                                           title = excluded.title,
                                           title_fold = excluded.title_fold,
                                           kind = excluded.kind,
                                           size_bytes = excluded.size_bytes,
                                           duration_ms = excluded.duration_ms,
                                           container = excluded.container,
                                           bitrate_bps = excluded.bitrate_bps,
                                           video_codec = excluded.video_codec,
                                           audio_codec = excluded.audio_codec,
                                           width = excluded.width,
                                           height = excluded.height,
                                           frame_rate_mille = excluded.frame_rate_mille,
                                           audio_channels = excluded.audio_channels,
                                           sample_rate = excluded.sample_rate,
                                           series_name = excluded.series_name,
                                           season_number = excluded.season_number,
                                           episode_number = excluded.episode_number,
                                           subtitle_tracks_json = excluded.subtitle_tracks_json,
                                           artist = excluded.artist,
                                           album = excluded.album,
                                           album_artist = excluded.album_artist,
                                           genre = excluded.genre,
                                           chapters_json = excluded.chapters_json,
                                           video_profile = excluded.video_profile,
                                           video_level = excluded.video_level,
                                           pixel_format = excluded.pixel_format,
                                           color_primaries = excluded.color_primaries,
                                           color_transfer = excluded.color_transfer,
                                           color_space = excluded.color_space,
                                           audio_tracks_json = excluded.audio_tracks_json,
                                           attachments_json = excluded.attachments_json,
                                           community_rating = excluded.community_rating,
                                           critic_rating = excluded.critic_rating,
                                           official_rating = excluded.official_rating,
                                           production_year = excluded.production_year,
                                           premiere_date = excluded.premiere_date,
                                           overview = excluded.overview,
                                           tagline = excluded.tagline,
                                           provider_ids = excluded.provider_ids,
                                           series_folder = excluded.series_folder,
                                           series_year = excluded.series_year,
                                           -- Preserve original
                                           -- created_at on rescans;
                                           -- COALESCE keeps existing
                                           -- value when row predates
                                           -- migration 0010.
                                           created_at = COALESCE(media_items.created_at, excluded.created_at)",
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
        .bind(p.width.map(|v| v as i64))
        .bind(p.height.map(|v| v as i64))
        .bind(p.frame_rate_mille.map(|v| v as i64))
        .bind(p.audio_channels.map(|v| v as i64))
        .bind(p.sample_rate.map(|v| v as i64))
        .bind(series_name)
        .bind(season_number.map(|v| v as i64))
        .bind(episode_number.map(|v| v as i64))
        .bind(subtitle_tracks_json)
        .bind(p.artist.as_deref())
        .bind(p.album.as_deref())
        .bind(p.album_artist.as_deref())
        .bind(p.genre.as_deref())
        .bind(created_at)
        .bind(chapters_json)
        .bind(p.video_profile.as_deref())
        .bind(p.video_level.map(|v| v as i64))
        .bind(p.pixel_format.as_deref())
        .bind(p.color_primaries.as_deref())
        .bind(p.color_transfer.as_deref())
        .bind(p.color_space.as_deref())
        .bind(crate::audio_track_json::encode(&p.audio_tracks))
        .bind(attachments_json)
        .bind(m.community_rating)
        .bind(m.critic_rating)
        .bind(m.official_rating.as_deref())
        .bind(m.production_year.map(|v| v as i64))
        .bind(m.premiere_date)
        .bind(m.overview.as_deref())
        .bind(m.tagline.as_deref())
        .bind(provider_ids_json)
        .bind(series_folder)
        .bind(series_year.map(|v| v as i64))
        // LIB-B2 — Unicode-case-folded title for SQL search + SortName.
        .bind(item.title.to_lowercase())
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
        // SQLite uses positional `?`; the placeholder index is irrelevant.
        let built = media_query::build(q, |_| "?".to_string(), "-1");
        let user = media_query::user_data_user(q);
        let join = if media_query::needs_user_data_join(q) {
            // LEFT JOIN so a missing user_data row → all-default flags
            // (COALESCEd in the WHERE). Keyed on the bound user id, which is
            // the FIRST bound parameter (prepended below).
            "LEFT JOIN user_data ud ON ud.item_id = media_items.id AND ud.user_id = ?"
        } else {
            ""
        };
        let where_clause = if built.where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", built.where_sql)
        };
        let limit_clause = built.limit_sql;
        // Window count so the page + TotalRecordCount come from one scan.
        let sql = format!(
            "SELECT {MEDIA_COLUMNS}, COUNT(*) OVER () AS total_count \
             FROM media_items {join} {where_clause} ORDER BY {} {limit_clause}",
            built.order_sql,
        );
        let mut query = sqlx::query_as::<_, QueryRow>(&sql);
        // The user-data join's user id binds FIRST (it appears in the FROM
        // clause, ahead of every WHERE/ORDER/LIMIT placeholder).
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
            // Empty page: the window count produced no row. Re-derive the
            // total with a dedicated COUNT so an over-offset (or
            // matches-nothing) page still reports the real
            // TotalRecordCount.
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
        // fts5 MATCH expression: each token a prefix term, AND-joined. The
        // tokens are alphanumeric-only (search_tokens), so no fts5 operator
        // can leak; still bound as a single parameter, never interpolated.
        let match_expr = tokens
            .iter()
            .map(|t| format!("{t}*"))
            .collect::<Vec<_>>()
            .join(" ");
        // Substring needle for the SUPERSET arm: covers the mid-word case
        // the tokenizer prefix can't reach (e.g. "kemon" inside "Pokemon").
        // Folded via title_fold (Unicode) like the rest of the search path.
        let needle = format!(
            "%{}%",
            crate::media_query::like_escape(&q.term.trim().to_lowercase())
        );
        let limit = i64::from(q.limit.max(1));
        let offset = i64::from(q.offset);
        let total = self.search_total(&match_expr, &needle, &q.kinds).await?;
        let rows = self
            .search_page(&match_expr, &needle, &q.kinds, limit, offset)
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
        let id_i64 =
            i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let row = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
            "SELECT last_scanned, file_mtime, file_size_seen, last_seen_scan_id \
             FROM media_items WHERE id = ?",
        )
        .bind(id_i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(last_scanned, file_mtime, file_size, last_seen)| ScanState {
                last_scanned: last_scanned.unwrap_or(0),
                file_mtime: file_mtime.unwrap_or(0),
                file_size: file_size.and_then(|v| u64::try_from(v).ok()).unwrap_or(0),
                last_seen_scan_id: last_seen.unwrap_or(0),
            },
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn begin_scan(&self, root: &std::path::Path) -> DomainResult<i64> {
        let root_str = root.to_str();
        let now = now_unix_secs();
        let row = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO scan_runs (root, started_at) VALUES (?, ?) RETURNING id",
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
        let id_i64 =
            i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let size_i64 =
            i64::try_from(size).map_err(|e| DomainError::Backend(format!("size overflow: {e}")))?;
        let now = now_unix_secs();
        sqlx::query(
            "UPDATE media_items SET file_mtime = ?, file_size_seen = ?, \
             last_scanned = ?, last_seen_scan_id = ? WHERE id = ?",
        )
        .bind(mtime)
        .bind(size_i64)
        .bind(now)
        .bind(scan_id)
        .bind(id_i64)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(scan.id = scan_id))]
    async fn sweep_unseen(&self, scan_id: i64, root_prefix: &str) -> DomainResult<Vec<MediaId>> {
        // Root-scoped, single atomic DELETE (V10). The pattern matches only
        // items strictly under `root_prefix` (path-separator boundary), so a
        // sibling root sharing a string prefix (/media/movies vs
        // /media/movies-4k) is never swept; wildcards in the root are escaped.
        let like = crate::root_like_pattern(root_prefix);
        let rows = sqlx::query_as::<_, (i64,)>(
            "DELETE FROM media_items \
             WHERE (last_seen_scan_id IS NULL OR last_seen_scan_id != ?) \
               AND path LIKE ? ESCAPE '\\' \
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
            "UPDATE scan_runs SET finished_at = ?, items_seen = ?, items_swept = ? \
             WHERE id = ?",
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
        // Raw 8 bytes bound as a BLOB; first match by ascending id.
        let sql = format!(
            "SELECT {MEDIA_COLUMNS} FROM media_items \
             WHERE fingerprint = ? ORDER BY id LIMIT 1"
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
        let id_i64 =
            i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        sqlx::query("UPDATE media_items SET fingerprint = ? WHERE id = ?")
            .bind(fp.as_slice())
            .bind(id_i64)
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(media.id = %id, media.path = %new_path.display()))]
    async fn rebind_path(&self, id: MediaId, new_path: &std::path::Path) -> DomainResult<()> {
        let id_i64 =
            i64::try_from(id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        // UPDATE-only: keeps the id (and every user_data FK) intact, just
        // repoints the path of a moved/renamed file. Zero rows when absent.
        sqlx::query("UPDATE media_items SET path = ? WHERE id = ?")
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
        let id_i64 = i64::try_from(item_id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        // One row per (item, role): a re-scan that discovers the same/winning
        // sidecar overwrites the locator + source rather than duplicating.
        sqlx::query(
            "INSERT INTO artwork (item_id, role, source, locator) VALUES (?, ?, ?, ?) \
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
        let id_i64 = i64::try_from(item_id)
            .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT role, source, locator FROM artwork WHERE item_id = ? ORDER BY role",
        )
        .bind(id_i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows)
    }
}

impl GenreStore for SqliteStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_genre(&self, name: &str) -> DomainResult<i64> {
        let wire_id = pharos_core::genre_wire_id(name);
        // INSERT-OR-IGNORE on the UNIQUE name, then read the id back —
        // works whether the row was just created or already existed.
        sqlx::query(
            "INSERT INTO genres (name, wire_id) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
        )
        .bind(name)
        .bind(&wire_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let (id,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM genres WHERE name = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_genres(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        // Normalise: trim, drop blanks, de-dup — the same set the
        // /Genres list + ParentId pivot resolve against.
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
        // Replace this item's links wholesale so a rescan that dropped a
        // genre doesn't leave a stale join row.
        sqlx::query("DELETE FROM item_genres WHERE item_id = ?")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for name in &wanted {
            let wire_id = pharos_core::genre_wire_id(name);
            sqlx::query(
                "INSERT INTO genres (name, wire_id) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (gid,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM genres WHERE name = ?")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO item_genres (item_id, genre_id) VALUES (?, ?) \
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
        let rows = sqlx::query_as::<_, (i64, String, String, i64)>(
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
                genre: Genre { id, name, wire_id },
                item_count: count.max(0) as u32,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_genre(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT ig.item_id FROM item_genres ig \
             JOIN genres g ON g.id = ig.genre_id \
             WHERE g.wire_id = ? ORDER BY ig.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn backfill_genres(&self) -> DomainResult<u64> {
        // Read every (id, genre) pair, split the genre string, and
        // re-link. link_item_genres is idempotent so this is safe to run
        // repeatedly (e.g. lazily on the first /Genres after upgrade).
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

impl PersonStore for SqliteStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_person(
        &self,
        name: &str,
        sort_name: Option<&str>,
        provider_ids: Option<&str>,
        thumb_url: Option<&str>,
    ) -> DomainResult<i64> {
        let wire_id = pharos_core::person_wire_id(name);
        // INSERT-OR-IGNORE on the UNIQUE name; on conflict refresh the
        // optional columns ONLY when a new value is supplied (COALESCE the
        // excluded value over the existing one) so a later scan that
        // learned the headshot fills it without clobbering with NULL.
        sqlx::query(
            "INSERT INTO people (name, sort_name, wire_id, provider_ids, thumb_url) \
             VALUES (?, ?, ?, ?, ?) \
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
        let (id,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM people WHERE name = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id)
    }

    #[tracing::instrument(skip(self, people), fields(media.id = %item))]
    async fn link_item_people(&self, item: MediaId, people: &[PersonRef]) -> DomainResult<()> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        // Normalise: trim names, drop blanks, de-dup on (name, role token).
        // The PK is (item_id, person_id, role) so the same person may hold
        // two roles, but an identical (name, role) twice is collapsed.
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
        // Wholesale replace this item's credits so a rescan that dropped a
        // person doesn't leave a stale join row.
        sqlx::query("DELETE FROM item_people WHERE item_id = ?")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for p in &wanted {
            let name = p.name.trim();
            let wire_id = pharos_core::person_wire_id(name);
            sqlx::query(
                "INSERT INTO people (name, sort_name, wire_id, provider_ids, thumb_url) \
                 VALUES (?, ?, ?, ?, ?) \
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
            let (pid,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM people WHERE name = ?")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let role = p.role.as_deref().unwrap_or("").trim();
            let sort_order = p.sort_order.map(|n| n as i64);
            sqlx::query(
                "INSERT INTO item_people \
                     (item_id, person_id, role, character, person_kind, sort_order) \
                 VALUES (?, ?, ?, ?, ?, ?) \
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
                i64,
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
                        id,
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
                i64,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<String>,
            ),
        >(
            "SELECT id, name, sort_name, wire_id, provider_ids, thumb_url \
             FROM people WHERE wire_id = ? LIMIT 1",
        )
        .bind(wire_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(
            |(id, name, sort_name, wire_id, provider_ids, thumb_url)| Person {
                id,
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
             WHERE p.wire_id = ? ORDER BY ip.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn people_for_item(&self, item: MediaId) -> DomainResult<Vec<ItemPerson>> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let rows =
            sqlx::query_as::<_, (String, String, String, Option<String>, String, Option<i64>)>(
                "SELECT p.name, p.wire_id, ip.role, ip.character, ip.person_kind, ip.sort_order \
             FROM item_people ip JOIN people p ON p.id = ip.person_id \
             WHERE ip.item_id = ? \
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

impl StudioStore for SqliteStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_studio(&self, name: &str) -> DomainResult<i64> {
        let wire_id = pharos_core::studio_wire_id(name);
        // INSERT-OR-IGNORE on the UNIQUE name, then read the id back —
        // works whether the row was just created or already existed.
        sqlx::query(
            "INSERT INTO studios (name, wire_id) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
        )
        .bind(name)
        .bind(&wire_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let (id,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM studios WHERE name = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_studios(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        // Normalise: trim, drop blanks, de-dup — the same set the
        // /Studios list + ParentId pivot resolve against.
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
        // Replace this item's links wholesale so a rescan that dropped a
        // studio doesn't leave a stale join row.
        sqlx::query("DELETE FROM item_studios WHERE item_id = ?")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for name in &wanted {
            let wire_id = pharos_core::studio_wire_id(name);
            sqlx::query(
                "INSERT INTO studios (name, wire_id) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (sid,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM studios WHERE name = ?")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO item_studios (item_id, studio_id) VALUES (?, ?) \
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
        let rows = sqlx::query_as::<_, (i64, String, String, i64)>(
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
                studio: Studio { id, name, wire_id },
                item_count: count.max(0) as u32,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_studio(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT is_.item_id FROM item_studios is_ \
             JOIN studios s ON s.id = is_.studio_id \
             WHERE s.wire_id = ? ORDER BY is_.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn studios_for_item(&self, item: MediaId) -> DomainResult<Vec<Studio>> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT s.id, s.name, s.wire_id FROM item_studios is_ \
             JOIN studios s ON s.id = is_.studio_id \
             WHERE is_.item_id = ? ORDER BY s.name",
        )
        .bind(item_i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, wire_id)| Studio { id, name, wire_id })
            .collect())
    }
}

impl TagStore for SqliteStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_tag(&self, name: &str) -> DomainResult<i64> {
        let wire_id = pharos_core::tag_wire_id(name);
        sqlx::query("INSERT INTO tags (name, wire_id) VALUES (?, ?) ON CONFLICT(name) DO NOTHING")
            .bind(name)
            .bind(&wire_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let (id,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM tags WHERE name = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_tags(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        // Normalise: trim, drop blanks, de-dup — the same set the /Tags
        // list + ParentId pivot resolve against.
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
        // Replace this item's links wholesale so a rescan that dropped a
        // tag doesn't leave a stale join row.
        sqlx::query("DELETE FROM item_tags WHERE item_id = ?")
            .bind(item_i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        for name in &wanted {
            let wire_id = pharos_core::tag_wire_id(name);
            sqlx::query(
                "INSERT INTO tags (name, wire_id) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (tid,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM tags WHERE name = ?")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO item_tags (item_id, tag_id) VALUES (?, ?) \
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
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
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
        // Incremental add: leave the item's existing tags untouched, only
        // upsert + link the named ones (PK conflict ignored).
        let mut added = 0u64;
        for name in &wanted {
            let wire_id = pharos_core::tag_wire_id(name);
            sqlx::query(
                "INSERT INTO tags (name, wire_id) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (tid,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM tags WHERE name = ?")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            let res = sqlx::query(
                "INSERT INTO item_tags (item_id, tag_id) VALUES (?, ?) \
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
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
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
            // Drop only the join link (the tag row may serve other items).
            let res = sqlx::query(
                "DELETE FROM item_tags WHERE item_id = ? \
                 AND tag_id = (SELECT id FROM tags WHERE name = ?)",
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
        let rows = sqlx::query_as::<_, (i64, String, String, i64)>(
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
                tag: Tag { id, name, wire_id },
                item_count: count.max(0) as u32,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_tag(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT it.item_id FROM item_tags it \
             JOIN tags t ON t.id = it.tag_id \
             WHERE t.wire_id = ? ORDER BY it.item_id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn tags_for_item(&self, item: MediaId) -> DomainResult<Vec<Tag>> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT t.id, t.name, t.wire_id FROM item_tags it \
             JOIN tags t ON t.id = it.tag_id \
             WHERE it.item_id = ? ORDER BY t.name",
        )
        .bind(item_i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, wire_id)| Tag { id, name, wire_id })
            .collect())
    }
}

impl CollectionStore for SqliteStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_collection(
        &self,
        name: &str,
        kind: Option<&str>,
        overview: Option<&str>,
    ) -> DomainResult<i64> {
        let wire_id = pharos_core::collection_wire_id(name);
        // Default kind is 'boxset' (the schema default); a None kind on a
        // fresh insert uses it. On conflict refresh kind/overview ONLY when
        // a new value is supplied (COALESCE excluded over existing) so an
        // NFO rescan doesn't wipe an operator's manual overview with NULL.
        sqlx::query(
            "INSERT INTO collections (name, wire_id, kind, overview) \
             VALUES (?, ?, COALESCE(?, 'boxset'), ?) \
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
        let (id,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM collections WHERE name = ?")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id)
    }

    #[tracing::instrument(skip(self, names), fields(media.id = %item))]
    async fn link_item_collections(&self, item: MediaId, names: &[String]) -> DomainResult<()> {
        let item_i64 =
            i64::try_from(item).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
        // Normalise: trim, drop blanks, de-dup.
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
                "INSERT INTO collections (name, wire_id, kind) VALUES (?, ?, 'boxset') \
                 ON CONFLICT(name) DO NOTHING",
            )
            .bind(name)
            .bind(&wire_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            let (cid,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM collections WHERE name = ?")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| DomainError::Backend(e.to_string()))?;
            // Append after the current max sort_order so NFO membership keeps
            // a stable scan order. Idempotent: a PK conflict (item already a
            // member) leaves the existing row + order untouched.
            let (next_order,) = sqlx::query_as::<_, (i64,)>(
                "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM collection_items \
                 WHERE collection_id = ?",
            )
            .bind(cid)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
            sqlx::query(
                "INSERT INTO collection_items (collection_id, item_id, sort_order) \
                 VALUES (?, ?, ?) ON CONFLICT(collection_id, item_id) DO NOTHING",
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
        let rows = sqlx::query_as::<_, (i64, String, String, String, Option<String>, i64)>(
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
                        id,
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
        let row = sqlx::query_as::<_, (i64, String, String, String, Option<String>)>(
            "SELECT id, name, wire_id, kind, overview FROM collections WHERE wire_id = ? LIMIT 1",
        )
        .bind(wire_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(row.map(|(id, name, wire_id, kind, overview)| Collection {
            id,
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
             WHERE c.wire_id = ? ORDER BY ci.sort_order, ci.item_id",
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
        // Seed members in the supplied order. add_collection_items appends
        // after the current max (idempotent), preserving order across a
        // re-create of an existing-name set.
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
            sqlx::query_as::<_, (i64,)>("SELECT id FROM collections WHERE wire_id = ?")
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
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM collection_items WHERE collection_id = ?",
        )
        .bind(cid)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        let mut added = 0u64;
        // De-dup the request set so two of the same id in one call don't both
        // claim a sort slot; PK conflict still guards against existing rows.
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for item in item_ids {
            let item_i64 = i64::try_from(*item)
                .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
            if !seen.insert(item_i64) {
                continue;
            }
            let res = sqlx::query(
                "INSERT INTO collection_items (collection_id, item_id, sort_order) \
                 VALUES (?, ?, ?) ON CONFLICT(collection_id, item_id) DO NOTHING",
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
            sqlx::query_as::<_, (i64,)>("SELECT id FROM collections WHERE wire_id = ?")
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
            let item_i64 = i64::try_from(*item)
                .map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
            let res =
                sqlx::query("DELETE FROM collection_items WHERE collection_id = ? AND item_id = ?")
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

impl LibraryStore for SqliteStore {
    #[tracing::instrument(skip(self))]
    async fn upsert_library(
        &self,
        name: &str,
        root_path: &str,
        kind: LibraryKind,
        wire_id: &str,
    ) -> DomainResult<i64> {
        let now = now_unix_secs();
        // Upsert on the UNIQUE root_path: re-running with changed config
        // refreshes the name/kind/wire_id but preserves created_at + id.
        sqlx::query(
            "INSERT INTO libraries (name, root_path, kind, wire_id, created_at) \
             VALUES (?, ?, ?, ?, ?) \
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
        let (id,) = sqlx::query_as::<_, (i64,)>("SELECT id FROM libraries WHERE root_path = ?")
            .bind(root_path)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(id)
    }

    #[tracing::instrument(skip(self))]
    async fn delete_library(&self, root_path: &str) -> DomainResult<()> {
        sqlx::query("DELETE FROM libraries WHERE root_path = ?")
            .bind(root_path)
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn libraries(&self) -> DomainResult<Vec<Library>> {
        let rows = sqlx::query_as::<_, (i64, String, String, String, String)>(
            "SELECT id, name, root_path, kind, wire_id FROM libraries ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, root_path, kind, wire_id)| Library {
                id,
                name,
                root_path,
                kind: LibraryKind::parse(&kind),
                wire_id,
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn backfill_library_ids(&self) -> DomainResult<u64> {
        // Path-boundary-safe: for each library, stamp every media_items row
        // strictly under its root (root + '/'). root_like_pattern escapes
        // wildcards + appends "/%", so /media/movies never claims
        // /media/movies-4k. Longest-root-wins is not needed (roots don't
        // nest in practice); a later root simply overwrites if it does.
        let libs = self.libraries().await?;
        let mut total: i64 = 0;
        for lib in &libs {
            let like = crate::root_like_pattern(&lib.root_path);
            sqlx::query("UPDATE media_items SET library_id = ? WHERE path LIKE ? ESCAPE '\\'")
                .bind(lib.id)
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
        total += count;
        Ok(total.max(0) as u64)
    }

    #[tracing::instrument(skip(self))]
    async fn item_ids_for_library(&self, wire_id: &str) -> DomainResult<Vec<MediaId>> {
        let rows = sqlx::query_as::<_, (i64,)>(
            "SELECT m.id FROM media_items m \
             JOIN libraries l ON l.id = m.library_id \
             WHERE l.wire_id = ? ORDER BY m.id",
        )
        .bind(wire_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DomainError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as MediaId).collect())
    }
}

/// LIB-B1 — one page row: a flattened [`MediaRow`] plus the window
/// `COUNT(*) OVER ()` total that rides along on every row of the page.
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
    width: Option<i64>,
    height: Option<i64>,
    frame_rate_mille: Option<i64>,
    audio_channels: Option<i64>,
    sample_rate: Option<i64>,
    series_name: Option<String>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    subtitle_tracks_json: Option<String>,
    attachments_json: Option<String>,
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
            artist: self.artist,
            album: self.album,
            album_artist: self.album_artist,
            genre: self.genre,
            chapters: crate::chapter_json::decode(self.chapters_json.as_deref()),
            // P34 — alternate editions land via a future scanner
            // enrichment pass. Today's persisted probes never carry
            // them so default to an empty Vec.
            alternate_sources: Vec::new(),
        };
        let series = self.series_name.map(|name| SeriesInfo {
            series_name: name,
            season_number: self.season_number.and_then(|v| u32::try_from(v).ok()),
            episode_number: self.episode_number.and_then(|v| u32::try_from(v).ok()),
            series_folder: self.series_folder,
            series_year: self.series_year.and_then(|v| u32::try_from(v).ok()),
        });
        let created_at = self.created_at;
        let metadata = MediaMetadata {
            community_rating: self.community_rating.map(|v| v as f32),
            critic_rating: self.critic_rating.map(|v| v as f32),
            official_rating: self.official_rating,
            production_year: self.production_year.and_then(|v| u32::try_from(v).ok()),
            premiere_date: self.premiere_date,
            overview: self.overview,
            tagline: self.tagline,
            provider_ids: crate::provider_ids_json::decode(self.provider_ids.as_deref()),
        };
        Ok(MediaItem {
            id,
            path: self.path.into(),
            title: self.title,
            kind,
            probe,
            series,
            created_at,
            metadata,
        })
    }
}
