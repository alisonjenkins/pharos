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
    AuthError, AuthResult, AuthToken, DomainError, DomainResult, MediaId, MediaItem, MediaKind,
    MediaProbe, MediaStore, SecretString, SeriesInfo, TokenStore, UserDataStore, UserId,
    UserItemData, UserPolicy, UserRecord, UserStore,
};

const MEDIA_COLUMNS: &str = "id, path, title, kind, size_bytes, duration_ms, container, \
    bitrate_bps, video_codec, audio_codec, width, height, frame_rate_mille, \
    audio_channels, sample_rate, series_name, season_number, episode_number, \
    subtitle_tracks_json, artist, album, album_artist, genre, created_at, chapters_json, \
    video_profile, video_level, pixel_format, color_primaries, color_transfer, color_space, \
    audio_tracks_json";
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

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

    /// Read or initialise this server's stable identity UUID. Mirrors
    /// `SqliteStore::load_or_create_server_id` (T35).
    pub async fn load_or_create_server_id(&self) -> Result<String, StoreError> {
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
}

impl PostgresStore {
    pub async fn load_runtime_config(&self) -> Result<crate::RuntimeConfig, StoreError> {
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

    pub async fn set_runtime_config(&self, cfg: &crate::RuntimeConfig) -> Result<(), StoreError> {
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
        let subtitle_tracks_json = crate::subtitle_track_json::encode(&p.subtitle_tracks);
        let chapters_json = crate::chapter_json::encode(&p.chapters);
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
                audio_tracks_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, \
                     $28, $29, $30, $31, $32)
             ON CONFLICT (id) DO UPDATE SET path = EXCLUDED.path,
                                            title = EXCLUDED.title,
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
        let token = Uuid::new_v4().simple().to_string();
        let user_bytes = user_id.0.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO auth_tokens (token, user_id, device_id, created_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(&token)
        .bind(user_bytes)
        .bind(device_id)
        .bind(now_unix_secs())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(AuthToken(SecretString::new(token)))
    }

    #[tracing::instrument(skip(self, token))]
    async fn resolve(&self, token: &str) -> AuthResult<UserId> {
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT user_id FROM auth_tokens WHERE token = $1")
                .bind(token)
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
        sqlx::query("DELETE FROM auth_tokens WHERE token = $1")
            .bind(token)
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
            artist: self.artist,
            album: self.album,
            album_artist: self.album_artist,
            genre: self.genre,
            chapters: crate::chapter_json::decode(self.chapters_json.as_deref()),
            // P34 — alternate editions enrichment lives in the
            // scanner; postgres rows today never carry them.
            alternate_sources: Vec::new(),
        };
        let series = self.series_name.map(|name| SeriesInfo {
            series_name: name,
            season_number: self.season_number.and_then(|v| u32::try_from(v).ok()),
            episode_number: self.episode_number.and_then(|v| u32::try_from(v).ok()),
        });
        Ok(MediaItem {
            id,
            path: self.path.into(),
            title: self.title,
            kind,
            probe,
            series,
            created_at: self.created_at,
        })
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
