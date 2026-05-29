use crate::StoreError;
use pharos_core::{
    DomainError, DomainResult, MediaId, MediaItem, MediaKind, MediaProbe, MediaStore, SeriesInfo,
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use std::str::FromStr;

const MEDIA_COLUMNS: &str = "id, path, title, kind, size_bytes, duration_ms, container, \
    bitrate_bps, video_codec, audio_codec, width, height, frame_rate_mille, \
    audio_channels, sample_rate, series_name, season_number, episode_number, \
    subtitle_tracks_json, artist, album, album_artist, genre, created_at, chapters_json, \
    video_profile, video_level, pixel_format, color_primaries, color_transfer, color_space";

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open a pool against the given sqlx connect URL (e.g. `sqlite::memory:`,
    /// `sqlite:///var/lib/pharos/data.db`). Runs migrations to latest.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(StoreError::Sqlx)?
            .create_if_missing(true)
            .foreign_keys(true);
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
        let subtitle_tracks_json = crate::subtitle_track_json::encode(&p.subtitle_tracks);
        let chapters_json = crate::chapter_json::encode(&p.chapters);
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
                pixel_format, color_primaries, color_transfer, color_space) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET path = excluded.path,
                                           title = excluded.title,
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
            artist: self.artist,
            album: self.album,
            album_artist: self.album_artist,
            genre: self.genre,
            chapters: crate::chapter_json::decode(self.chapters_json.as_deref()),
        };
        let series = self.series_name.map(|name| SeriesInfo {
            series_name: name,
            season_number: self.season_number.and_then(|v| u32::try_from(v).ok()),
            episode_number: self.episode_number.and_then(|v| u32::try_from(v).ok()),
        });
        let created_at = self.created_at;
        Ok(MediaItem {
            id,
            path: self.path.into(),
            title: self.title,
            kind,
            probe,
            series,
            created_at,
        })
    }
}
