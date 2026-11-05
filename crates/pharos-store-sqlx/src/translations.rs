//! `MetadataTranslationStore` for both backends (ADR-0062, migration 0066).
//! One row per (show or episode, language) of provider text; the default
//! `overview` columns are untouched here.

use pharos_core::{DomainError, DomainResult, LocalizedText, TranslationGap, TranslationOwner};

const UPSERT: &str = "INSERT INTO metadata_translations \
       (owner_kind, owner_key, language, title, overview, is_original, fetched_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7) \
     ON CONFLICT (owner_kind, owner_key, language) DO UPDATE SET \
       title = excluded.title, overview = excluded.overview, \
       is_original = excluded.is_original, fetched_at = excluded.fetched_at";

/// A row counts as present when it carries text, or when it records a miss
/// newer than the cutoff (`$3`); an older miss is asked about again.
const PRESENT: &str = "(t.title IS NOT NULL OR t.overview IS NOT NULL OR t.fetched_at >= $3)";

/// Item rows whose media item is gone, run after a sweep deletes items.
/// The item id is stable across a move or rescan, so only a swept item
/// leaves rows behind. Series rows key on `series_metadata.series_key`,
/// whose rows nothing deletes, so they have no orphans to clear.
pub(crate) const DELETE_ORPHAN_ITEM_ROWS: &str = "DELETE FROM metadata_translations \
     WHERE owner_kind = 'item' AND NOT EXISTS (SELECT 1 FROM media_items m \
       WHERE m.id = CAST(metadata_translations.owner_key AS BIGINT))";

fn series_gaps_sql() -> String {
    format!(
        "SELECT sm.series_key, sm.match_external_id FROM series_metadata sm \
         WHERE sm.match_provider = $1 \
           AND sm.match_external_id IS NOT NULL AND sm.match_external_id <> '' \
           AND (sm.match_source IS NULL OR sm.match_source <> 'none') \
           AND NOT EXISTS (SELECT 1 FROM metadata_translations t \
             WHERE t.owner_kind = 'series' AND t.owner_key = sm.series_key \
               AND t.language = $2 AND {PRESENT}) \
         ORDER BY sm.series_key LIMIT $4"
    )
}

/// The season/episode numbers are INT4 on postgres, which sqlx won't decode
/// as i64, so the query widens them.
fn episode_gaps_sql() -> String {
    format!(
        "SELECT m.id, m.match_external_id, CAST(m.season_number AS BIGINT), \
           CAST(m.episode_number AS BIGINT) FROM media_items m \
         WHERE m.kind = 'episode' AND m.match_provider = $1 \
           AND m.match_external_id IS NOT NULL AND m.match_external_id <> '' \
           AND m.match_source IS NOT NULL AND m.match_source <> 'none' \
           AND m.season_number IS NOT NULL AND m.episode_number IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM metadata_translations t \
             WHERE t.owner_kind = 'item' AND t.owner_key = CAST(m.id AS TEXT) \
               AND t.language = $2 AND {PRESENT}) \
         ORDER BY m.id LIMIT $4"
    )
}

fn backend(e: sqlx::Error) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn owner_parts(owner: &TranslationOwner) -> (&'static str, String) {
    match owner {
        TranslationOwner::Item(id) => ("item", id.to_string()),
        TranslationOwner::Series(key) => ("series", key.clone()),
    }
}

fn owner_from(kind: &str, key: String) -> Option<TranslationOwner> {
    match kind {
        "item" => key.parse().ok().map(TranslationOwner::Item),
        "series" => Some(TranslationOwner::Series(key)),
        _ => None,
    }
}

/// `SELECT` for [`translations_for`]: `$1..$n` are the owner keys of one
/// kind, then the languages. `with_original` adds the base-record row.
fn select_sql(kind: &str, owners: usize, languages: usize, with_original: bool) -> String {
    let keys = (1..=owners).map(|i| format!("${i}")).collect::<Vec<_>>();
    let langs = (owners + 1..=owners + languages)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>();
    let mut wanted = Vec::new();
    if !langs.is_empty() {
        wanted.push(format!("language IN ({})", langs.join(",")));
    }
    if with_original {
        wanted.push("is_original".to_string());
    }
    format!(
        "SELECT owner_kind, owner_key, language, title, overview, is_original \
         FROM metadata_translations WHERE owner_kind = '{kind}' \
         AND owner_key IN ({}) AND ({})",
        keys.join(","),
        wanted.join(" OR ")
    )
}

/// Owner keys bound per `SELECT`. A long episode list is read in chunks so
/// the query stays far under sqlite's 32766 and postgres's 65535 binds.
const OWNERS_PER_QUERY: usize = 500;

type Row = (String, String, String, Option<String>, Option<String>, bool);

fn rows_into(rows: Vec<Row>) -> Vec<(TranslationOwner, LocalizedText)> {
    rows.into_iter()
        .filter_map(|(kind, key, language, title, overview, is_original)| {
            Some((
                owner_from(&kind, key)?,
                LocalizedText {
                    language,
                    title,
                    overview,
                    is_original,
                },
            ))
        })
        .collect()
}

fn gaps(
    provider: &str,
    language: &str,
    series: Vec<(String, String)>,
    episodes: Vec<(i64, String, Option<i64>, Option<i64>)>,
) -> Vec<TranslationGap> {
    let series = series.into_iter().map(|(key, ext)| TranslationGap {
        owner: TranslationOwner::Series(key),
        provider: provider.to_string(),
        series_external_id: ext,
        season: None,
        episode: None,
        language: language.to_string(),
    });
    let episodes = episodes.into_iter().filter_map(|(id, ext, s, e)| {
        Some(TranslationGap {
            owner: TranslationOwner::Item(u64::try_from(id).ok()?),
            provider: provider.to_string(),
            series_external_id: ext,
            season: s.and_then(|v| u32::try_from(v).ok()),
            episode: e.and_then(|v| u32::try_from(v).ok()),
            language: language.to_string(),
        })
    });
    series.chain(episodes).collect()
}

fn remaining(limit: i64, taken: usize) -> i64 {
    limit
        .saturating_sub(i64::try_from(taken).unwrap_or(i64::MAX))
        .max(0)
}

/// The same body for both backends: every query above is written in the
/// `$n` placeholder form both sqlite and postgres accept.
macro_rules! impl_translation_store {
    ($store:ty) => {
        impl pharos_core::MetadataTranslationStore for $store {
            async fn upsert_translations(
                &self,
                owner: &TranslationOwner,
                texts: &[LocalizedText],
                fetched_at: i64,
            ) -> DomainResult<()> {
                if texts.is_empty() {
                    return Ok(());
                }
                let (kind, key) = owner_parts(owner);
                let mut tx = self.pool().begin().await.map_err(backend)?;
                for t in texts {
                    sqlx::query(UPSERT)
                        .bind(kind)
                        .bind(key.as_str())
                        .bind(t.language.as_str())
                        .bind(t.title.as_deref())
                        .bind(t.overview.as_deref())
                        .bind(t.is_original)
                        .bind(fetched_at)
                        .execute(&mut *tx)
                        .await
                        .map_err(backend)?;
                }
                tx.commit().await.map_err(backend)
            }

            async fn translations_for(
                &self,
                owners: &[TranslationOwner],
                languages: &[String],
                with_original: bool,
            ) -> DomainResult<Vec<(TranslationOwner, LocalizedText)>> {
                if owners.is_empty() || (languages.is_empty() && !with_original) {
                    return Ok(Vec::new());
                }
                let mut out = Vec::new();
                for kind in ["item", "series"] {
                    let keys: Vec<String> = owners
                        .iter()
                        .map(owner_parts)
                        .filter(|(k, _)| *k == kind)
                        .map(|(_, key)| key)
                        .collect();
                    for chunk in keys.chunks(OWNERS_PER_QUERY) {
                        let sql = select_sql(kind, chunk.len(), languages.len(), with_original);
                        let mut q = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql));
                        for k in chunk {
                            q = q.bind(k.as_str());
                        }
                        for l in languages {
                            q = q.bind(l.as_str());
                        }
                        out.extend(rows_into(q.fetch_all(self.pool()).await.map_err(backend)?));
                    }
                }
                Ok(out)
            }

            async fn translation_gaps(
                &self,
                provider: &str,
                language: &str,
                missing_cutoff: i64,
                limit: i64,
            ) -> DomainResult<Vec<TranslationGap>> {
                let series: Vec<(String, String)> =
                    sqlx::query_as(sqlx::AssertSqlSafe(series_gaps_sql()))
                        .bind(provider)
                        .bind(language)
                        .bind(missing_cutoff)
                        .bind(limit)
                        .fetch_all(self.pool())
                        .await
                        .map_err(backend)?;
                let episodes: Vec<(i64, String, Option<i64>, Option<i64>)> =
                    sqlx::query_as(sqlx::AssertSqlSafe(episode_gaps_sql()))
                        .bind(provider)
                        .bind(language)
                        .bind(missing_cutoff)
                        .bind(remaining(limit, series.len()))
                        .fetch_all(self.pool())
                        .await
                        .map_err(backend)?;
                Ok(gaps(provider, language, series, episodes))
            }
        }
    };
}

#[cfg(feature = "sqlite")]
impl_translation_store!(crate::sqlite::SqliteStore);

#[cfg(feature = "postgres")]
impl_translation_store!(crate::postgres::PostgresStore);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_numbers_owner_keys_before_languages() {
        let sql = select_sql("item", 2, 1, true);
        assert!(sql.contains("owner_key IN ($1,$2)"), "{sql}");
        assert!(sql.contains("language IN ($3) OR is_original"), "{sql}");
    }

    #[test]
    fn select_with_only_the_original() {
        let sql = select_sql("series", 1, 0, true);
        assert!(sql.contains("AND (is_original)"), "{sql}");
    }

    #[test]
    fn owner_round_trips_through_its_columns() {
        for owner in [
            TranslationOwner::Item(42),
            TranslationOwner::Series("TV/Dragon Ball".into()),
        ] {
            let (kind, key) = owner_parts(&owner);
            assert_eq!(owner_from(kind, key), Some(owner));
        }
        assert_eq!(owner_from("bogus", "1".into()), None);
    }
}
