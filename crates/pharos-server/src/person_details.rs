//! A person's biography, birth and death, and IMDb and Wikidata ids, as a
//! metadata provider (TMDB) knows them. Fetched when someone opens the
//! person's page and kept in the store, so the provider is asked about a
//! person at most once a month.
//!
//! The fetch sits behind [`PersonDetailsSource`] (ADR-0084) so the provider
//! can be swapped, and faked in tests.

use std::time::Duration;

use futures_util::future::BoxFuture;
use pharos_core::{Person, PersonDetails, PersonStore as _};

use crate::state::AppState;

/// A page waits this long for the provider before showing what is kept.
const FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// Kept details older than this are fetched again: a biography changes
/// rarely, and a death date is the one fact worth catching within weeks.
const STALE_AFTER_SECS: i64 = 30 * 24 * 60 * 60;

/// What the provider says about one person.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    pub biography: Option<String>,
    pub birth_date: Option<String>,
    pub death_date: Option<String>,
    pub birth_place: Option<String>,
    pub imdb_id: Option<String>,
    pub wikidata_id: Option<String>,
}

/// Why a fetch gave nothing. Its [`label`](Self::label) is an `outcome` of
/// `pharos_person_details_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailsMiss {
    /// The provider has no such person: kept as an empty record.
    NotFound,
    Transport,
    Status(u16),
    Decode,
}

impl DetailsMiss {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Transport => "transport",
            Self::Status(_) => "status",
            Self::Decode => "decode",
        }
    }
}

/// Where a person's details come from, by the provider's own person id.
pub trait PersonDetailsSource: Send + Sync {
    fn fetch<'a>(&'a self, tmdb_id: &'a str) -> BoxFuture<'a, Result<Fetched, DetailsMiss>>;
}

impl PersonDetailsSource for crate::tmdb::TmdbClient {
    fn fetch<'a>(&'a self, tmdb_id: &'a str) -> BoxFuture<'a, Result<Fetched, DetailsMiss>> {
        Box::pin(self.person_detail(tmdb_id))
    }
}

/// How [`current`] answered. The `outcome` label of
/// `pharos_person_details_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Kept details young enough to show as they are.
    Kept,
    Fetched,
    /// The person has no TMDB id to ask about.
    NoProviderId,
    /// No provider is configured (no TMDB key).
    NoSource,
    Timeout,
    Miss(DetailsMiss),
    StoreError,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::Fetched => "fetched",
            Self::NoProviderId => "no_provider_id",
            Self::NoSource => "no_source",
            Self::Timeout => "timeout",
            Self::Miss(miss) => miss.label(),
            Self::StoreError => "store_error",
        }
    }
}

/// The TMDB person id in a person row's `provider_ids` (`tmdb:1234`, alone
/// or among others separated by commas or spaces).
pub fn tmdb_id(provider_ids: Option<&str>) -> Option<&str> {
    provider_ids?
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .find_map(|token| token.trim().strip_prefix("tmdb:"))
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// `id` when it is an IMDb person id (`nm` and digits); the UI builds a link
/// from it, so nothing else is let through.
fn imdb_person_id(id: Option<String>) -> Option<String> {
    id.filter(|id| {
        id.strip_prefix("nm")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// `id` when it is a Wikidata item id (`Q` and digits).
fn wikidata_item_id(id: Option<String>) -> Option<String> {
    id.filter(|id| {
        id.strip_prefix('Q')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn kept_from(fetched: Fetched, now: i64) -> PersonDetails {
    let text = |s: Option<String>| s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    PersonDetails {
        biography: text(fetched.biography),
        birth_date: text(fetched.birth_date),
        death_date: text(fetched.death_date),
        birth_place: text(fetched.birth_place),
        imdb_id: imdb_person_id(fetched.imdb_id),
        wikidata_id: wikidata_item_id(fetched.wikidata_id),
        fetched_at: now,
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `person`'s details: those kept, fetched first when there are none or they
/// are stale and a provider knows the person. A failed fetch shows what was
/// kept (or nothing) and is tried again on the next visit; a provider that no
/// longer knows the person leaves what was kept as it was. Counted under
/// `pharos_person_details_total{outcome}`.
pub async fn current(state: &AppState, person: &Person) -> Option<PersonDetails> {
    let (details, outcome) = resolve(state, person, now_secs()).await;
    metrics::counter!("pharos_person_details_total", "outcome" => outcome.label()).increment(1);
    match outcome {
        Outcome::Timeout
        | Outcome::Miss(DetailsMiss::Transport | DetailsMiss::Status(_) | DetailsMiss::Decode) => {
            tracing::warn!(person.wire_id = %person.wire_id, outcome = outcome.label(), ?outcome, "person details: provider fetch failed");
        }
        _ => {
            tracing::debug!(person.wire_id = %person.wire_id, outcome = outcome.label(), "person details")
        }
    }
    details
}

async fn resolve(state: &AppState, person: &Person, now: i64) -> (Option<PersonDetails>, Outcome) {
    let kept = match state.stores.person_details(&person.wire_id).await {
        Ok(kept) => kept,
        Err(e) => {
            tracing::warn!(person.wire_id = %person.wire_id, error = %e, "person details: store read failed");
            return (None, Outcome::StoreError);
        }
    };
    if kept
        .as_ref()
        .is_some_and(|k| now.saturating_sub(k.fetched_at) < STALE_AFTER_SECS)
    {
        return (kept, Outcome::Kept);
    }
    let Some(source) = state.person_details.as_deref() else {
        return (kept, Outcome::NoSource);
    };
    let Some(id) = tmdb_id(person.provider_ids.as_deref()) else {
        return (kept, Outcome::NoProviderId);
    };
    let fetched = match tokio::time::timeout(FETCH_TIMEOUT, source.fetch(id)).await {
        Err(_) => return (kept, Outcome::Timeout),
        // A provider that has lost the person keeps what we already had.
        Ok(Err(DetailsMiss::NotFound)) if kept.is_some() => {
            let renewed = kept.map(|old| PersonDetails {
                fetched_at: now,
                ..old
            });
            if let Some(renewed) = &renewed {
                if let Err(e) = state
                    .stores
                    .put_person_details(&person.wire_id, renewed)
                    .await
                {
                    tracing::warn!(person.wire_id = %person.wire_id, error = %e, "person details: store write failed");
                }
            }
            return (renewed, Outcome::Miss(DetailsMiss::NotFound));
        }
        Ok(Err(DetailsMiss::NotFound)) => Fetched::default(),
        Ok(Err(miss)) => return (kept, Outcome::Miss(miss)),
        Ok(Ok(fetched)) => fetched,
    };
    let outcome = if fetched == Fetched::default() {
        Outcome::Miss(DetailsMiss::NotFound)
    } else {
        Outcome::Fetched
    };
    let details = kept_from(fetched, now);
    if let Err(e) = state
        .stores
        .put_person_details(&person.wire_id, &details)
        .await
    {
        tracing::warn!(person.wire_id = %person.wire_id, error = %e, "person details: store write failed");
    }
    (Some(details), outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tmdb_id_is_found_among_other_provider_ids() {
        assert_eq!(tmdb_id(Some("tmdb:1234")), Some("1234"));
        assert_eq!(tmdb_id(Some("tvdb:9, tmdb:77")), Some("77"));
        assert_eq!(tmdb_id(Some("tvdb:9")), None);
        assert_eq!(tmdb_id(Some("tmdb:12ab")), None);
        assert_eq!(tmdb_id(None), None);
    }

    #[test]
    fn only_well_formed_ids_are_kept() {
        let fetched = Fetched {
            biography: Some("  Born by the sea.  ".into()),
            birth_place: Some("  ".into()),
            imdb_id: Some("nm0000042".into()),
            wikidata_id: Some("Q42".into()),
            ..Fetched::default()
        };
        let kept = kept_from(fetched, 9);
        assert_eq!(kept.biography.as_deref(), Some("Born by the sea."));
        assert_eq!(kept.birth_place, None);
        assert_eq!(kept.imdb_id.as_deref(), Some("nm0000042"));
        assert_eq!(kept.wikidata_id.as_deref(), Some("Q42"));
        assert_eq!(kept.fetched_at, 9);
        let odd = Fetched {
            imdb_id: Some("javascript:alert(1)".into()),
            wikidata_id: Some("Q".into()),
            ..Fetched::default()
        };
        let kept = kept_from(odd, 9);
        assert_eq!((kept.imdb_id, kept.wikidata_id), (None, None));
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let labels: std::collections::HashSet<_> = [
            Outcome::Kept,
            Outcome::Fetched,
            Outcome::NoProviderId,
            Outcome::NoSource,
            Outcome::Timeout,
            Outcome::Miss(DetailsMiss::NotFound),
            Outcome::Miss(DetailsMiss::Transport),
            Outcome::Miss(DetailsMiss::Status(500)),
            Outcome::Miss(DetailsMiss::Decode),
            Outcome::StoreError,
        ]
        .map(Outcome::label)
        .into_iter()
        .collect();
        assert_eq!(labels.len(), 10);
    }
}
