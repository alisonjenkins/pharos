//! The one place that knows how a [`MediaItem`] becomes a `media_items` row on
//! Postgres. `put` writes one row and `put_batch` writes many through the same
//! column list, conflict clause and bind sequence, so the two cannot drift.

use pharos_core::{DomainError, DomainResult, MediaItem};
use sqlx::postgres::PgArguments;
use sqlx::query::Query;
use sqlx::Postgres;

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
    audio_tracks_json, \
    community_rating, critic_rating, official_rating, production_year, \
    premiere_date, overview, tagline, provider_ids, \
    series_folder, series_year, title_fold, attachments_json, \
    track_number, disc_number, release_year, \
    production_locations_json, trailers_json, \
    synopsis, content_rating, network, release_date, original_language, \
    book_format, book_page_count, book_author, book_publisher, \
    book_series, book_series_index, book_isbn, sort_name";

pub(crate) const PUT_COLUMN_COUNT: usize = 62;

/// `created_at` keeps the value already stored (rows that predate migration
/// 0010 have none, hence the COALESCE).
pub(crate) const PUT_CONFLICT_UPDATE: &str = "path = EXCLUDED.path,
    title = EXCLUDED.title,
    title_fold = EXCLUDED.title_fold,
    sort_name = EXCLUDED.sort_name,
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
    original_language = EXCLUDED.original_language,
    book_format = EXCLUDED.book_format,
    book_page_count = EXCLUDED.book_page_count,
    book_author = EXCLUDED.book_author,
    book_publisher = EXCLUDED.book_publisher,
    book_series = EXCLUDED.book_series,
    book_series_index = EXCLUDED.book_series_index,
    book_isbn = EXCLUDED.book_isbn,
    trailers_json = EXCLUDED.trailers_json,
    synopsis = EXCLUDED.synopsis,
    content_rating = EXCLUDED.content_rating,
    network = EXCLUDED.network,
    release_date = EXCLUDED.release_date,
    created_at = COALESCE(media_items.created_at, EXCLUDED.created_at)";

/// `INSERT .. VALUES (..), (..) ON CONFLICT (id) DO UPDATE` for `rows` rows.
/// The caller must not repeat an id within one statement: Postgres refuses to
/// let one `ON CONFLICT DO UPDATE` touch a row twice.
pub(crate) fn upsert_sql(rows: usize) -> String {
    let values = (0..rows)
        .map(|r| {
            let params = (1..=PUT_COLUMN_COUNT)
                .map(|c| format!("${}", r * PUT_COLUMN_COUNT + c))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({params})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO media_items ({PUT_COLUMNS}) VALUES {values} \
         ON CONFLICT (id) DO UPDATE SET {PUT_CONFLICT_UPDATE}"
    )
}

type Q<'q> = Query<'q, Postgres, PgArguments>;

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
    let created_at = item.created_at.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
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
        .bind(p.synopsis.as_deref())
        .bind(p.content_rating.as_deref())
        .bind(p.network.as_deref())
        .bind(p.release_date.as_deref())
        .bind(item.metadata.original_language.clone())
        // 004-books — seven columns, all NULL for a non-book item. Every bind
        // is derived from the SAME Option<BookMeta>, so a book can never be
        // half-written. Postgres placeholder arity is not compile-checked, which
        // is why `just test-postgres` is mandatory after touching this.
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
        .bind(pharos_core::sort_name(&item.title)))
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
                "created_at = COALESCE(media_items.created_at, EXCLUDED.created_at)".to_string()
            } else {
                format!("{col} = EXCLUDED.{col}")
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
    fn upsert_sql_numbers_placeholders_row_after_row() {
        let sql = upsert_sql(2);
        assert!(sql.contains("($1, $2,"));
        assert!(sql.contains(&format!(
            "(${}, ${},",
            PUT_COLUMN_COUNT + 1,
            PUT_COLUMN_COUNT + 2
        )));
        assert!(sql.contains(&format!("${})", 2 * PUT_COLUMN_COUNT)));
        assert!(!sql.contains(&format!("${}", 2 * PUT_COLUMN_COUNT + 1)));
    }
}
