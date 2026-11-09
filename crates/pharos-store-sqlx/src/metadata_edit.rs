//! `MetadataEditStore` for both backends (spec 015 US3, ADR-0097, migration
//! 0078). The `locked_fields` columns are written here and nowhere else; the
//! scan's put and the series enricher's upsert read them to keep a locked
//! column's stored value (V1030).

#[cfg(test)]
use pharos_core::EditableField;
use pharos_core::{DomainError, DomainResult, EditTarget, LockedFields, MetadataEdit};

/// The bits whose lock keeps a `media_items` column: a sort title derived
/// from a locked title is kept with it, as the edit wrote it.
#[cfg(test)]
pub(crate) fn item_column_guard(col: &str) -> Option<u32> {
    let f = item_column_field(col)?;
    Some(match f {
        EditableField::SortName => f.bit() | EditableField::Title.bit(),
        _ => f.bit(),
    })
}

/// The `media_items` column(s) an [`EditableField`] lives in, for the put's
/// conflict-clause tests. `title_fold` follows `title`: it is the title's
/// search key.
#[cfg(test)]
pub(crate) fn item_column_field(col: &str) -> Option<EditableField> {
    use EditableField as F;
    Some(match col {
        "title" | "title_fold" => F::Title,
        "sort_name" => F::SortName,
        "overview" => F::Overview,
        "tagline" => F::Tagline,
        "production_year" => F::ProductionYear,
        "premiere_date" => F::PremiereDate,
        "official_rating" => F::OfficialRating,
        "community_rating" => F::CommunityRating,
        "critic_rating" => F::CriticRating,
        _ => return None,
    })
}

/// One statement for every item edit: each column is rewritten only when its
/// flag is set. A new title also moves an unlocked sort name with it, the way
/// a put derives one.
const EDIT_ITEM: &str = "UPDATE media_items SET \
       title = CASE WHEN $1 THEN $2 ELSE title END, \
       title_fold = CASE WHEN $1 THEN $3 ELSE title_fold END, \
       sort_name = CASE WHEN $4 THEN $5 \
           WHEN $1 AND (locked_fields & 2) = 0 THEN $6 ELSE sort_name END, \
       overview = CASE WHEN $7 THEN $8 ELSE overview END, \
       tagline = CASE WHEN $9 THEN $10 ELSE tagline END, \
       production_year = CASE WHEN $11 THEN $12 ELSE production_year END, \
       premiere_date = CASE WHEN $13 THEN $14 ELSE premiere_date END, \
       official_rating = CASE WHEN $15 THEN $16 ELSE official_rating END, \
       community_rating = CASE WHEN $17 THEN $18 ELSE community_rating END, \
       critic_rating = CASE WHEN $19 THEN $20 ELSE critic_rating END, \
       locked_fields = (locked_fields | CAST($21 AS BIGINT)) & ~CAST($22 AS BIGINT) \
     WHERE id = $23 \
     RETURNING locked_fields";

/// A show may have no metadata row yet (never enriched), so the edit creates
/// one; the enricher later fills the rest and keeps what is locked.
const EDIT_SERIES: &str = "INSERT INTO series_metadata (series_key, series_name, \
       overview, premiere_date, official_rating, community_rating, genres, locked_fields) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS BIGINT) & ~CAST($9 AS BIGINT)) \
     ON CONFLICT(series_key) DO UPDATE SET \
       overview = CASE WHEN $10 THEN excluded.overview ELSE series_metadata.overview END, \
       premiere_date = CASE WHEN $11 THEN excluded.premiere_date \
           ELSE series_metadata.premiere_date END, \
       official_rating = CASE WHEN $12 THEN excluded.official_rating \
           ELSE series_metadata.official_rating END, \
       community_rating = CASE WHEN $13 THEN excluded.community_rating \
           ELSE series_metadata.community_rating END, \
       genres = CASE WHEN $14 THEN excluded.genres ELSE series_metadata.genres END, \
       locked_fields = (series_metadata.locked_fields | CAST($8 AS BIGINT)) \
           & ~CAST($9 AS BIGINT) \
     RETURNING locked_fields";

fn backend(e: sqlx::Error) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn item_id(id: pharos_core::MediaId) -> DomainResult<i64> {
    i64::try_from(id).map_err(|e| DomainError::Backend(format!("media id {id}: {e}")))
}

/// The bound values of one item edit, decoded once for both backends.
struct ItemBinds<'a> {
    title: Option<&'a str>,
    title_fold: Option<String>,
    derived_sort: Option<String>,
    sort_name: Option<Option<&'a str>>,
    overview: Option<Option<&'a str>>,
    tagline: Option<Option<&'a str>>,
    official_rating: Option<Option<&'a str>>,
    set: i64,
    unlock: i64,
}

impl<'a> ItemBinds<'a> {
    fn of(edit: &'a MetadataEdit) -> Self {
        let title = edit.title.as_deref();
        Self {
            title,
            title_fold: title.map(str::to_lowercase),
            derived_sort: title.map(pharos_core::sort_name),
            sort_name: edit.sort_name.as_ref().map(Option::as_deref),
            overview: edit.overview.as_ref().map(Option::as_deref),
            tagline: edit.tagline.as_ref().map(Option::as_deref),
            official_rating: edit.official_rating.as_ref().map(Option::as_deref),
            set: edit.fields().without(edit.unlock).bits(),
            unlock: edit.unlock.bits(),
        }
    }
}

/// The series edit's genres as stored: only written when the edit sets them,
/// but the column is NOT NULL, so a fresh row gets an empty list.
fn series_genres(edit: &MetadataEdit) -> String {
    edit.genres
        .as_deref()
        .and_then(crate::string_list_json::encode)
        .unwrap_or_else(|| "[]".into())
}

fn not_found(target: &EditTarget) -> DomainError {
    match target {
        EditTarget::Item(id) => DomainError::NotFound(*id),
        EditTarget::Series { key, .. } => {
            DomainError::Backend(format!("series {key}: the edit upsert returned no row"))
        }
    }
}

macro_rules! edit_impl {
    ($store:ty, $year:ty, $rating:ty) => {
        impl pharos_core::MetadataEditStore for $store {
            #[tracing::instrument(skip(self, edit), fields(edit.set = edit.fields().bits(), edit.unlock = edit.unlock.bits()))]
            async fn edit_metadata(
                &self,
                target: &EditTarget,
                edit: &MetadataEdit,
            ) -> DomainResult<LockedFields> {
                let year = |v: Option<u32>| v.and_then(|y| <$year>::try_from(y).ok());
                let rating = |v: Option<f32>| v.map(<$rating>::from);
                let locked: Option<i64> = match target {
                    EditTarget::Item(id) => {
                        let b = ItemBinds::of(edit);
                        sqlx::query_scalar(EDIT_ITEM)
                            .bind(b.title.is_some())
                            .bind(b.title)
                            .bind(b.title_fold)
                            .bind(b.sort_name.is_some())
                            .bind(b.sort_name.flatten())
                            .bind(b.derived_sort)
                            .bind(b.overview.is_some())
                            .bind(b.overview.flatten())
                            .bind(b.tagline.is_some())
                            .bind(b.tagline.flatten())
                            .bind(edit.production_year.is_some())
                            .bind(year(edit.production_year.flatten()))
                            .bind(edit.premiere_date.is_some())
                            .bind(edit.premiere_date.flatten())
                            .bind(b.official_rating.is_some())
                            .bind(b.official_rating.flatten())
                            .bind(edit.community_rating.is_some())
                            .bind(rating(edit.community_rating.flatten()))
                            .bind(edit.critic_rating.is_some())
                            .bind(rating(edit.critic_rating.flatten()))
                            .bind(b.set)
                            .bind(b.unlock)
                            .bind(item_id(*id)?)
                            .fetch_optional(self.pool())
                            .await
                            .map_err(backend)?
                    }
                    EditTarget::Series { key, name } => {
                        let set = edit.fields().without(edit.unlock);
                        sqlx::query_scalar(EDIT_SERIES)
                            .bind(key.as_str())
                            .bind(name.as_str())
                            .bind(edit.overview.clone().flatten())
                            .bind(edit.premiere_date.flatten())
                            .bind(edit.official_rating.clone().flatten())
                            .bind(rating(edit.community_rating.flatten()))
                            .bind(series_genres(edit))
                            .bind(set.bits())
                            .bind(edit.unlock.bits())
                            .bind(edit.overview.is_some())
                            .bind(edit.premiere_date.is_some())
                            .bind(edit.official_rating.is_some())
                            .bind(edit.community_rating.is_some())
                            .bind(edit.genres.is_some())
                            .fetch_optional(self.pool())
                            .await
                            .map_err(backend)?
                    }
                };
                locked
                    .map(LockedFields::from_bits)
                    .ok_or_else(|| not_found(target))
            }

            async fn stored_sort_name(
                &self,
                id: pharos_core::MediaId,
            ) -> DomainResult<Option<String>> {
                let row: Option<Option<String>> =
                    sqlx::query_scalar("SELECT sort_name FROM media_items WHERE id = $1")
                        .bind(item_id(id)?)
                        .fetch_optional(self.pool())
                        .await
                        .map_err(backend)?;
                Ok(row.flatten())
            }
        }
    };
}

#[cfg(feature = "sqlite")]
edit_impl!(crate::sqlite::SqliteStore, i64, f64);

#[cfg(feature = "postgres")]
edit_impl!(crate::postgres::PostgresStore, i32, f32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_field_has_a_column() {
        let cols = [
            "title",
            "sort_name",
            "overview",
            "tagline",
            "production_year",
            "premiere_date",
            "official_rating",
            "community_rating",
            "critic_rating",
        ];
        for f in EditableField::ALL.into_iter().filter(|f| f.on_item()) {
            assert!(
                cols.iter().any(|c| item_column_field(c) == Some(f)),
                "{} has no media_items column",
                f.label()
            );
        }
    }

    /// The series upsert's guards are written out in each backend's SQL;
    /// this ties their bit numbers to [`EditableField::bit`].
    #[test]
    fn the_series_upsert_guards_every_series_field() {
        for (name, src) in [
            ("sqlite.rs", include_str!("sqlite.rs")),
            ("postgres.rs", include_str!("postgres.rs")),
        ] {
            for f in EditableField::ALL.into_iter().filter(|f| f.on_series()) {
                let col = match f {
                    EditableField::Genres => "genres".to_string(),
                    other => other.label().to_string(),
                };
                let guard = format!(
                    "{col} = CASE WHEN (series_metadata.locked_fields & {}) <> 0",
                    f.bit()
                );
                assert!(src.contains(&guard), "{name}: no `{guard}`");
            }
        }
    }

    #[test]
    fn the_sort_name_guard_names_the_sort_name_bit() {
        assert!(EDIT_ITEM.contains(&format!(
            "(locked_fields & {}) = 0",
            EditableField::SortName.bit()
        )));
    }
}
