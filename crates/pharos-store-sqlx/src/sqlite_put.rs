//! The one place that knows how a [`MediaItem`] becomes a `media_items` row on
//! SQLite. `put` writes one row and `put_batch` writes many through the same
//! column list, conflict clause and bind sequence, so the two cannot drift.

use pharos_core::{DomainError, DomainResult, MediaItem};
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Columns `put` writes, in the order [`bind_item`] binds them.
pub(crate) const PUT_COLUMNS: &str = "id, path, title, kind, \
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
    production_locations_json, trailers_json, \
    series_folder, series_year, track_number, disc_number, release_year, \
    synopsis, content_rating, network, release_date, title_fold, \
    original_language, \
    book_format, book_page_count, book_author, book_publisher, \
    book_series, book_series_index, book_isbn, sort_name, alternate_sources_json, video_3d_format, has_3d";

pub(crate) const PUT_COLUMN_COUNT: usize = 65;

/// Rows per `INSERT`. SQLite caps bound variables at 32766 per statement.
pub(crate) const MAX_ROWS_PER_STATEMENT: usize = 500;
const _: () = assert!(MAX_ROWS_PER_STATEMENT * PUT_COLUMN_COUNT <= 32_766);

/// `created_at` keeps the value already stored (rows that predate migration
/// 0010 have none, hence the COALESCE).
pub(crate) const PUT_CONFLICT_UPDATE: &str = "path = excluded.path,
    title = excluded.title,
    title_fold = excluded.title_fold,
    sort_name = excluded.sort_name,
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
    production_locations_json = excluded.production_locations_json,
    original_language = excluded.original_language,
    book_format = excluded.book_format,
    book_page_count = excluded.book_page_count,
    book_author = excluded.book_author,
    book_publisher = excluded.book_publisher,
    book_series = excluded.book_series,
    book_series_index = excluded.book_series_index,
    book_isbn = excluded.book_isbn,
    alternate_sources_json = excluded.alternate_sources_json,
    video_3d_format = excluded.video_3d_format,
    has_3d = excluded.has_3d,
    trailers_json = excluded.trailers_json,
    series_folder = excluded.series_folder,
    series_year = excluded.series_year,
    track_number = excluded.track_number,
    disc_number = excluded.disc_number,
    release_year = excluded.release_year,
    synopsis = excluded.synopsis,
    content_rating = excluded.content_rating,
    network = excluded.network,
    release_date = excluded.release_date,
    created_at = COALESCE(media_items.created_at, excluded.created_at)";

/// `INSERT .. VALUES (..), (..) ON CONFLICT(id) DO UPDATE` for `rows` rows.
pub(crate) fn upsert_sql(rows: usize) -> String {
    let row = format!("({})", vec!["?"; PUT_COLUMN_COUNT].join(", "));
    let values = vec![row.as_str(); rows].join(", ");
    format!(
        "INSERT INTO media_items ({PUT_COLUMNS}) VALUES {values} \
         ON CONFLICT(id) DO UPDATE SET {PUT_CONFLICT_UPDATE}"
    )
}

type Q<'q> = Query<'q, Sqlite, SqliteArguments>;

/// Bind one item's [`PUT_COLUMN_COUNT`] values, in [`PUT_COLUMNS`] order.
pub(crate) fn bind_item<'q>(q: Q<'q>, item: &'q MediaItem) -> DomainResult<Q<'q>> {
    let id_i64 =
        i64::try_from(item.id).map_err(|e| DomainError::Backend(format!("id overflow: {e}")))?;
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
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Prefer the caller-supplied created_at (round-tripping after
    // a get → put), fall back to "now" on first insert.
    let created_at = item.created_at.unwrap_or(now_secs);
    let has_3d = p.video_3d_format.is_some()
        || p.alternate_sources
            .iter()
            .any(|a| a.video_3d_format.is_some());
    Ok(q.bind(id_i64)
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
        .bind(production_locations_json)
        .bind(trailers_json)
        .bind(series_folder)
        .bind(series_year.map(|v| v as i64))
        .bind(p.track_number.map(|v| v as i64))
        .bind(p.disc_number.map(|v| v as i64))
        .bind(p.year.map(|v| v as i64))
        .bind(p.synopsis.as_deref())
        .bind(p.content_rating.as_deref())
        .bind(p.network.as_deref())
        .bind(p.release_date.as_deref())
        // LIB-B2 — Unicode-case-folded title for SQL search + SortName.
        .bind(item.title.to_lowercase())
        .bind(item.metadata.original_language.clone())
        // 004-books — seven columns, all NULL for a non-book item. `as_ref()`
        // on the Option<BookMeta> keeps every bind derived from the SAME
        // Option, so a book can never be half-written.
        .bind(item.book.as_ref().map(|b| b.format.as_str()))
        .bind(
            item.book
                .as_ref()
                .and_then(|b| b.page_count)
                .map(|v| v as i64),
        )
        .bind(item.book.as_ref().and_then(|b| b.author.clone()))
        .bind(item.book.as_ref().and_then(|b| b.publisher.clone()))
        .bind(item.book.as_ref().and_then(|b| b.series_name.clone()))
        .bind(
            item.book
                .as_ref()
                .and_then(|b| b.series_index)
                .map(|v| v as i64),
        )
        .bind(item.book.as_ref().and_then(|b| b.isbn.clone()))
        .bind(pharos_core::sort_name(&item.title))
        .bind(crate::alternate_source_json::encode(&p.alternate_sources))
        .bind(p.video_3d_format.map(pharos_core::Video3DFormat::wire_name))
        .bind(i64::from(has_3d)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<&'static str> {
        PUT_COLUMNS.split(',').map(str::trim).collect()
    }

    #[test]
    fn column_count_constant_matches_the_list() {
        assert_eq!(columns().len(), PUT_COLUMN_COUNT);
    }

    #[test]
    fn every_column_but_the_key_is_updated_on_conflict() {
        for col in columns().into_iter().filter(|c| *c != "id") {
            let assign = if col == "created_at" {
                "created_at = COALESCE(media_items.created_at, excluded.created_at)".to_string()
            } else {
                format!("{col} = excluded.{col}")
            };
            assert!(
                PUT_CONFLICT_UPDATE.contains(&assign),
                "{col} is inserted but not refreshed on conflict"
            );
        }
    }

    #[test]
    fn nothing_but_insert_columns_is_updated_on_conflict() {
        let cols = columns();
        for assign in PUT_CONFLICT_UPDATE.split(",\n") {
            let target = assign.split(" = ").next().unwrap_or_default().trim();
            assert!(cols.contains(&target), "{target} is not an insert column");
        }
    }

    #[test]
    fn upsert_sql_has_one_placeholder_group_per_row() {
        let sql = upsert_sql(3);
        assert_eq!(sql.matches('?').count(), 3 * PUT_COLUMN_COUNT);
    }
}
