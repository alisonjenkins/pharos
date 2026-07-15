//! T81 — person-image backfill.
//!
//! A one-pass background sweep that resolves real cast portraits from TMDB
//! and writes the resulting public CDN URL onto each `people.thumb_url`. It
//! only runs when a TMDB API key is configured (see [`crate::tmdb`]); with no
//! key `main` never calls [`spawn`] and the feature is off.
//!
//! Pass semantics: pull every person whose portrait is unresolved
//! (`thumb_url` NULL or a legacy non-`http` path), resolve each by name, and
//! `upsert_person` the CDN URL. A resolved `http(s)` URL excludes the row
//! from `people_needing_images`, so the pass is self-terminating and a later
//! process restart safely re-tries only the still-unresolved rows (e.g. a
//! name TMDB had no match for last time). Each resolve draws a permit from
//! the shared `bg_io` gate, so the sweep paces itself against live playback
//! exactly like the trickplay / intro-detection sweeps (V34).

use crate::bg_io::BgPermit;
use crate::state::Stores;
use crate::tmdb::PersonImageResolver;
use pharos_core::{DomainResult, PersonStore};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Max people pulled in one pass. This library carries ~850 people; one pass
/// covers it. The cap bounds the query's memory and stops a pathological
/// library from materialising an unbounded result set.
const MAX_PER_PASS: i64 = 5_000;

/// Courtesy delay between TMDB calls — well under TMDB's published rate
/// ceiling so a full backfill never trips limiting. This is on top of the
/// `bg_io` gate (which throttles against playback, not the remote API).
const REQUEST_SPACING: Duration = Duration::from_millis(120);

/// Spawn the one-pass backfill on the tokio runtime. Fire-and-forget: a
/// failure aborts only this sweep (logged), never the server.
pub fn spawn<R>(stores: Stores, bg_io: Arc<Semaphore>, resolver: R)
where
    R: PersonImageResolver + Send + Sync + 'static,
{
    tokio::spawn(async move {
        match run(&stores, &bg_io, &resolver).await {
            Ok(n) => tracing::info!(resolved = n, "T81 person-image backfill: complete"),
            Err(e) => tracing::warn!(error = %e, "T81 person-image backfill: aborted"),
        }
    });
}

/// Run one backfill pass, returning how many portraits were newly resolved.
/// Extracted from [`spawn`] so it's directly awaitable in tests with a fake
/// resolver + an in-memory store.
pub(crate) async fn run<R>(
    stores: &Stores,
    bg_io: &Arc<Semaphore>,
    resolver: &R,
) -> DomainResult<usize>
where
    R: PersonImageResolver,
{
    let people = stores.people_needing_images(MAX_PER_PASS).await?;
    let total = people.len();
    if total == 0 {
        return Ok(0);
    }
    tracing::info!(total, "T81 person-image backfill: resolving portraits");
    let mut resolved = 0usize;
    for person in people {
        // Pace against live playback: hold a gate slot only for the fetch,
        // then free it before the courtesy sleep so an idle slot isn't parked.
        let maybe_url = {
            let _permit = BgPermit::acquire(bg_io).await;
            resolver.resolve(&person.name).await
        };
        if let Some(url) = maybe_url {
            // COALESCE-on-conflict upsert: fills thumb_url without touching
            // the other columns (see `upsert_person`).
            stores
                .upsert_person(&person.name, None, None, Some(&url))
                .await?;
            resolved += 1;
        }
        tokio::time::sleep(REQUEST_SPACING).await;
    }
    Ok(resolved)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_store_sqlx::sqlite::SqliteStore;

    /// Resolves every name to a deterministic CDN-shaped URL — no network.
    struct FakeResolver;
    impl PersonImageResolver for FakeResolver {
        async fn resolve(&self, name: &str) -> Option<String> {
            (!name.is_empty())
                .then(|| format!("https://image.tmdb.org/t/p/w300/{}.jpg", name.len()))
        }
    }

    /// A resolver that never finds a match — the row must stay unresolved and
    /// remain eligible for a later pass.
    struct MissResolver;
    impl PersonImageResolver for MissResolver {
        async fn resolve(&self, _name: &str) -> Option<String> {
            None
        }
    }

    async fn store() -> SqliteStore {
        SqliteStore::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite")
    }

    #[tokio::test]
    async fn fills_thumb_url_and_is_self_terminating() {
        let s = store().await;
        // A legacy local path counts as "needs image" (not http(s)).
        s.upsert_person("Jane Doe", None, None, Some("/config/metadata/jane.jpg"))
            .await
            .unwrap();
        s.upsert_person("John Roe", None, None, None).await.unwrap();
        let gate = Arc::new(Semaphore::new(8));

        let n = run(&s, &gate, &FakeResolver).await.unwrap();
        assert_eq!(n, 2, "both unresolved people get a portrait");

        // Both now carry an http(s) URL → excluded from the next pass.
        let n2 = run(&s, &gate, &FakeResolver).await.unwrap();
        assert_eq!(n2, 0, "resolved rows are not re-fetched");

        let jane = s
            .person_by_wire_id(&pharos_core::person_wire_id("Jane Doe"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            jane.thumb_url.as_deref().unwrap().starts_with("https://"),
            "legacy path replaced by the resolved CDN url"
        );
    }

    #[tokio::test]
    async fn no_match_leaves_row_eligible_for_retry() {
        let s = store().await;
        s.upsert_person("Nobody", None, None, None).await.unwrap();
        let gate = Arc::new(Semaphore::new(8));

        let n = run(&s, &gate, &MissResolver).await.unwrap();
        assert_eq!(n, 0, "no match → nothing written");

        // Still needs an image, so a later pass (e.g. after TMDB adds one)
        // can resolve it.
        let still = s.people_needing_images(10).await.unwrap();
        assert_eq!(still.len(), 1);
        assert_eq!(still[0].name, "Nobody");
    }
}
