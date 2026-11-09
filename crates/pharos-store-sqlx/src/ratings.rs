//! `RatingsStore` for both backends (B341, migration 0063). The ratings
//! columns are written here and nowhere else: a scan's put and the series
//! enricher's upsert never name them, so a rescan keeps a lookup's scores.

use pharos_core::{
    DomainError, DomainResult, ExternalRatings, MediaId, RatingsSubject, RatingsTarget,
};

/// The years are INT4 on postgres, which sqlx won't decode as i64, so both
/// queries widen them. Shows first: there are far fewer of them, and the TV
/// half of the ratings would otherwise wait on every film. Never-looked-up
/// rows lead each half.
const SERIES_DUE: &str = "SELECT sm.series_key, sm.series_name, sm.provider_ids, \
       CAST((SELECT MIN(m.series_year) FROM media_items m \
         WHERE m.kind = 'episode' AND COALESCE(m.series_key, m.series_folder, m.series_name) = sm.series_key) AS BIGINT) \
     FROM series_metadata sm \
     WHERE sm.ratings_fetched_at IS NULL OR sm.ratings_fetched_at < $1 \
     ORDER BY sm.ratings_fetched_at IS NOT NULL, sm.ratings_fetched_at, sm.series_key \
     LIMIT $2";

const FILMS_DUE: &str =
    "SELECT id, title, provider_ids, CAST(production_year AS BIGINT) FROM media_items \
     WHERE kind = 'movie' AND (ratings_fetched_at IS NULL OR ratings_fetched_at < $1) \
     ORDER BY ratings_fetched_at IS NOT NULL, ratings_fetched_at, id \
     LIMIT $2";

const SET_ITEM: &str = "UPDATE media_items \
     SET imdb_rating = $1, rt_tomatometer = $2, ratings_fetched_at = $3 WHERE id = $4";

const SET_SERIES: &str = "UPDATE series_metadata \
     SET imdb_rating = $1, rt_tomatometer = $2, ratings_fetched_at = $3 WHERE series_key = $4";

fn backend(e: sqlx::Error) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn subject(
    target: RatingsTarget,
    title: String,
    provider_ids: Option<&str>,
    year: Option<i64>,
) -> RatingsSubject {
    RatingsSubject {
        target,
        imdb_id: crate::provider_ids_json::decode(provider_ids)
            .imdb
            .filter(|id| id.starts_with("tt")),
        title,
        year: year.and_then(|y| u32::try_from(y).ok()),
    }
}

fn subjects(
    series: Vec<(String, String, Option<String>, Option<i64>)>,
    films: Vec<(i64, String, Option<String>, Option<i64>)>,
) -> Vec<RatingsSubject> {
    let series = series.into_iter().map(|(key, name, ids, year)| {
        subject(RatingsTarget::Series(key), name, ids.as_deref(), year)
    });
    let films = films.into_iter().filter_map(|(id, title, ids, year)| {
        let id = MediaId::try_from(id).ok()?;
        Some(subject(
            RatingsTarget::Item(id),
            title,
            ids.as_deref(),
            year,
        ))
    });
    series.chain(films).collect()
}

fn remaining(limit: i64, taken: usize) -> i64 {
    limit
        .saturating_sub(i64::try_from(taken).unwrap_or(i64::MAX))
        .max(0)
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use crate::sqlite::SqliteStore;

    impl pharos_core::RatingsStore for SqliteStore {
        async fn needing_ratings(
            &self,
            limit: i64,
            ttl_cutoff: i64,
        ) -> DomainResult<Vec<RatingsSubject>> {
            let series: Vec<(String, String, Option<String>, Option<i64>)> =
                sqlx::query_as(SERIES_DUE)
                    .bind(ttl_cutoff)
                    .bind(limit)
                    .fetch_all(self.pool())
                    .await
                    .map_err(backend)?;
            let films: Vec<(i64, String, Option<String>, Option<i64>)> = sqlx::query_as(FILMS_DUE)
                .bind(ttl_cutoff)
                .bind(remaining(limit, series.len()))
                .fetch_all(self.pool())
                .await
                .map_err(backend)?;
            Ok(subjects(series, films))
        }

        async fn set_ratings(
            &self,
            target: &RatingsTarget,
            ratings: ExternalRatings,
            fetched_at: i64,
        ) -> DomainResult<()> {
            let imdb = ratings.imdb.map(f64::from);
            let tomatometer = ratings.tomatometer.map(i64::from);
            let query = match target {
                RatingsTarget::Item(id) => sqlx::query(SET_ITEM)
                    .bind(imdb)
                    .bind(tomatometer)
                    .bind(fetched_at)
                    .bind(i64::try_from(*id).map_err(|e| DomainError::Backend(e.to_string()))?),
                RatingsTarget::Series(key) => sqlx::query(SET_SERIES)
                    .bind(imdb)
                    .bind(tomatometer)
                    .bind(fetched_at)
                    .bind(key.as_str()),
            };
            query.execute(self.pool()).await.map_err(backend)?;
            Ok(())
        }
    }
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use crate::postgres::PostgresStore;

    impl pharos_core::RatingsStore for PostgresStore {
        async fn needing_ratings(
            &self,
            limit: i64,
            ttl_cutoff: i64,
        ) -> DomainResult<Vec<RatingsSubject>> {
            let series: Vec<(String, String, Option<String>, Option<i64>)> =
                sqlx::query_as(SERIES_DUE)
                    .bind(ttl_cutoff)
                    .bind(limit)
                    .fetch_all(self.pool())
                    .await
                    .map_err(backend)?;
            let films: Vec<(i64, String, Option<String>, Option<i64>)> = sqlx::query_as(FILMS_DUE)
                .bind(ttl_cutoff)
                .bind(remaining(limit, series.len()))
                .fetch_all(self.pool())
                .await
                .map_err(backend)?;
            Ok(subjects(series, films))
        }

        async fn set_ratings(
            &self,
            target: &RatingsTarget,
            ratings: ExternalRatings,
            fetched_at: i64,
        ) -> DomainResult<()> {
            let tomatometer = ratings.tomatometer.map(i64::from);
            let query = match target {
                RatingsTarget::Item(id) => sqlx::query(SET_ITEM)
                    .bind(ratings.imdb)
                    .bind(tomatometer)
                    .bind(fetched_at)
                    .bind(i64::try_from(*id).map_err(|e| DomainError::Backend(e.to_string()))?),
                RatingsTarget::Series(key) => sqlx::query(SET_SERIES)
                    .bind(ratings.imdb)
                    .bind(tomatometer)
                    .bind(fetched_at)
                    .bind(key.as_str()),
            };
            query.execute(self.pool()).await.map_err(backend)?;
            Ok(())
        }
    }
}
