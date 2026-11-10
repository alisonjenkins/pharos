//! `GET /admin/media/duplicates` — films the library holds more than once
//! (spec 015 US9, V285). Read-only: the admin decides what to delete.
//!
//! Two rows are versions of one film when they share a provider id (tmdb,
//! imdb, or the enricher's match), or, for a row with no provider id, the
//! same normalised title and production year. A row whose title and year
//! match several differently-identified films stays out of all of them,
//! since remakes and namesakes share titles. Editions already grouped under
//! one row (`alternate_sources`) are one item, so never a duplicate.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, Responder};
use pharos_core::{MediaItem, MediaKind, MediaQuery, MediaStore};
use std::collections::{BTreeMap, HashMap};

use super::admin::require_admin;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct DuplicateGroupDto {
    title: String,
    production_year: Option<u32>,
    /// The provider id the group was matched on (`tmdb:603`), `None` when
    /// it was matched on title and year alone.
    provider_id: Option<String>,
    versions: Vec<DuplicateVersionDto>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct DuplicateVersionDto {
    /// Stringified like every other admin id (see `DamagedFileDto`).
    id: String,
    path: String,
    size_bytes: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    video_codec: Option<String>,
    /// Unix seconds the scanner first saw the file.
    date_added: Option<i64>,
    /// Editions grouped under this row, so the admin sees it holds more
    /// than one file.
    edition_count: usize,
}

pub(super) async fn duplicate_movies(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let query = MediaQuery {
        kinds: vec![MediaKind::Movie],
        ..Default::default()
    };
    let (movies, _) = MediaStore::query(&state.stores, &query)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("listing movies: {e}")))?;
    let groups = group_duplicates(&movies);
    Ok(web::Json(
        groups.into_iter().map(group_dto).collect::<Vec<_>>(),
    ))
}

fn group_dto(versions: Vec<&MediaItem>) -> DuplicateGroupDto {
    let first = versions.first().copied();
    DuplicateGroupDto {
        title: first.map(|m| m.title.clone()).unwrap_or_default(),
        production_year: first.and_then(|m| m.metadata.production_year),
        provider_id: versions
            .iter()
            .find_map(|m| provider_keys(m).into_iter().next()),
        versions: versions
            .into_iter()
            .map(|m| DuplicateVersionDto {
                id: m.id.to_string(),
                path: m.path.to_string_lossy().into_owned(),
                size_bytes: m.probe.size_bytes,
                width: m.probe.width,
                height: m.probe.height,
                video_codec: m.probe.video_codec.clone(),
                date_added: m.created_at,
                edition_count: m.probe.versions().count(),
            })
            .collect(),
    }
}

/// Every provider id the row carries, as `provider:id`, tmdb first. A blank
/// id (an NFO's empty `<tmdbid/>`) is no id, or it would join every such row.
fn provider_keys(m: &MediaItem) -> Vec<String> {
    let ids = &m.metadata.provider_ids;
    let matched = (m.match_provider.as_deref(), m.match_external_id.as_deref());
    let candidates = [
        (Some("tmdb"), ids.tmdb.as_deref()),
        (Some("imdb"), ids.imdb.as_deref()),
        matched,
    ];
    let mut keys: Vec<String> = Vec::new();
    for (provider, id) in candidates {
        let (Some(provider), Some(id)) = (provider, id) else {
            continue;
        };
        let (provider, id) = (provider.trim().to_lowercase(), id.trim());
        if provider.is_empty() || id.is_empty() {
            continue;
        }
        let key = format!("{provider}:{id}");
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The title as `SortName` orders it (case, leading article) with
/// punctuation dropped, plus the year: `None` matches only `None`.
fn title_key(m: &MediaItem) -> (String, Option<u32>) {
    let folded: String = pharos_core::sort_name(&m.title)
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let words: Vec<&str> = folded.split_whitespace().collect();
    (words.join(" "), m.metadata.production_year)
}

/// Groups of two or more rows that are one film, each sorted by id, the
/// groups sorted by title then year.
fn group_duplicates(movies: &[MediaItem]) -> Vec<Vec<&MediaItem>> {
    let mut sets = DisjointSets::new(movies.len());
    let mut by_provider: HashMap<String, usize> = HashMap::new();
    let mut by_title: HashMap<(String, Option<u32>), Vec<usize>> = HashMap::new();
    for (i, m) in movies.iter().enumerate() {
        for key in provider_keys(m) {
            let first = *by_provider.entry(key).or_insert(i);
            sets.union(first, i);
        }
        by_title.entry(title_key(m)).or_default().push(i);
    }
    for members in by_title.values() {
        let (identified, anonymous): (Vec<usize>, Vec<usize>) = members
            .iter()
            .partition(|&&i| movies.get(i).is_some_and(|m| !provider_keys(m).is_empty()));
        let mut films: Vec<usize> = identified.iter().map(|&i| sets.find(i)).collect();
        films.sort_unstable();
        films.dedup();
        // Several differently-identified films share this title and year:
        // an unidentified row cannot say which it is.
        let anchor = match films.as_slice() {
            [] => anonymous.first().copied(),
            [film] => Some(*film),
            _ => None,
        };
        if let Some(anchor) = anchor {
            for &i in &anonymous {
                sets.union(anchor, i);
            }
        } else {
            for pair in anonymous.windows(2) {
                if let [a, b] = pair {
                    sets.union(*a, *b);
                }
            }
        }
    }

    let mut groups: BTreeMap<usize, Vec<&MediaItem>> = BTreeMap::new();
    for (i, m) in movies.iter().enumerate() {
        groups.entry(sets.find(i)).or_default().push(m);
    }
    let mut out: Vec<Vec<&MediaItem>> = groups.into_values().filter(|g| g.len() > 1).collect();
    for g in &mut out {
        g.sort_unstable_by_key(|m| m.id);
    }
    out.sort_by_cached_key(|g| g.first().map(|m| title_key(m)));
    out
}

/// Union-find over row indices, with path halving.
struct DisjointSets {
    parent: Vec<usize>,
}

impl DisjointSets {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut i: usize) -> usize {
        while let Some(&p) = self.parent.get(i) {
            if p == i {
                break;
            }
            let grand = self.parent.get(p).copied().unwrap_or(p);
            if let Some(slot) = self.parent.get_mut(i) {
                *slot = grand;
            }
            i = grand;
        }
        i
    }

    /// Joins the two sets under the smaller root, so a group's root is its
    /// first row and the output order does not depend on union order.
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        let (keep, drop) = (ra.min(rb), ra.max(rb));
        if let Some(slot) = self.parent.get_mut(drop) {
            *slot = keep;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_core::{AlternateMediaSource, MediaMetadata, MediaProbe, ProviderIds};

    fn film(id: u64, title: &str, year: Option<u32>, tmdb: Option<&str>) -> MediaItem {
        MediaItem {
            id,
            path: format!("/media/Movies/{id}.mkv").into(),
            title: title.into(),
            kind: MediaKind::Movie,
            metadata: MediaMetadata {
                production_year: year,
                provider_ids: ProviderIds {
                    tmdb: tmdb.map(Into::into),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn ids(groups: &[Vec<&MediaItem>]) -> Vec<Vec<u64>> {
        groups
            .iter()
            .map(|g| g.iter().map(|m| m.id).collect())
            .collect()
    }

    #[test]
    fn rows_sharing_a_provider_id_are_one_film() {
        let movies = [
            film(1, "The Matrix", Some(1999), Some("603")),
            film(2, "Matrix, The (Remastered)", None, Some("603")),
            film(3, "Heat", Some(1995), Some("949")),
        ];
        assert_eq!(ids(&group_duplicates(&movies)), vec![vec![1, 2]]);
    }

    #[test]
    fn an_unidentified_copy_joins_the_film_with_its_title_and_year() {
        let movies = [
            film(
                1,
                "The Six Million Dollar Man: Wine, Women and War",
                Some(1973),
                Some("1"),
            ),
            film(
                2,
                "Six Million Dollar Man - Wine Women and War",
                Some(1973),
                None,
            ),
        ];
        assert_eq!(ids(&group_duplicates(&movies)), vec![vec![1, 2]]);
    }

    #[test]
    fn unidentified_copies_group_on_title_and_year() {
        let movies = [
            film(1, "Heat", Some(1995), None),
            film(2, "heat", Some(1995), None),
            film(3, "Heat", Some(1986), None),
            film(4, "Heat", None, None),
        ];
        assert_eq!(ids(&group_duplicates(&movies)), vec![vec![1, 2]]);
    }

    #[test]
    fn namesakes_with_different_provider_ids_stay_apart() {
        let movies = [
            film(1, "Crash", Some(2004), Some("1640")),
            film(2, "Crash", Some(2004), Some("9999")),
            film(3, "Crash", Some(2004), None),
        ];
        assert!(group_duplicates(&movies).is_empty());
    }

    #[test]
    fn a_row_with_editions_is_one_item() {
        let mut cut = film(1, "Blade Runner", Some(1982), Some("78"));
        cut.probe = MediaProbe {
            alternate_sources: vec![AlternateMediaSource {
                id: "final".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let movies = [cut, film(2, "Heat", Some(1995), None)];
        assert!(group_duplicates(&movies).is_empty());
    }

    #[test]
    fn a_match_id_links_rows_like_a_provider_id() {
        let mut a = film(1, "Alien", Some(1979), None);
        a.match_provider = Some("tmdb".into());
        a.match_external_id = Some("348".into());
        let b = film(2, "Alien (Director's Cut)", Some(2003), Some("348"));
        assert_eq!(ids(&group_duplicates(&[a, b])), vec![vec![1, 2]]);
    }

    #[test]
    fn a_blank_provider_id_is_no_id() {
        let movies = [
            film(1, "Heat", Some(1995), Some("")),
            film(2, "Alien", Some(1979), Some(" ")),
        ];
        assert!(group_duplicates(&movies).is_empty());
    }

    #[test]
    fn groups_are_ordered_by_title() {
        let movies = [
            film(5, "Zodiac", Some(2007), None),
            film(6, "Zodiac", Some(2007), None),
            film(7, "The Abyss", Some(1989), None),
            film(8, "Abyss", Some(1989), None),
        ];
        assert_eq!(
            ids(&group_duplicates(&movies)),
            vec![vec![7, 8], vec![5, 6]]
        );
    }
}
