//! B341 — IMDb and Rotten Tomatoes scores for films and shows.
//!
//! [`RatingsSource`] is the port; `omdb.rs` is the one adapter. [`spawn`]
//! runs the sweep: it asks the store which titles are due
//! ([`RatingsStore::needing_ratings`]: never looked up, or looked up more
//! than the TTL ago), looks each one up, and records what it found, a miss
//! included, so a title isn't asked about again until the TTL. A spent quota
//! or a refused key stops the sweep until the next UTC day without recording
//! anything, so no title is hidden for a TTL by an answer that wasn't about
//! it. The sweep runs only when `[omdb].api_key` is set.

use std::future::Future;
use std::time::Duration;

use pharos_core::{DomainResult, ExternalRatings, RatingsStore, RatingsSubject, RatingsTarget};

/// What a lookup is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleKind {
    Movie,
    Series,
}

/// One title to look up: by IMDb id when it has one, else by title and year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatingsQuery<'a> {
    pub kind: TitleKind,
    pub imdb_id: Option<&'a str>,
    pub title: &'a str,
    pub year: Option<u32>,
}

/// A lookup's outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    /// The title's scores; either may be absent.
    Found(ExternalRatings),
    /// The source has no such title.
    NotFound,
    /// Nothing more will succeed today (quota spent, key refused).
    Stop(String),
    /// This lookup failed (network, a server error); try it on a later pass.
    Failed(String),
}

/// Where scores come from. Implementations never panic on a bad answer.
pub trait RatingsSource: Send + Sync {
    fn lookup(&self, query: &RatingsQuery<'_>) -> impl Future<Output = Lookup> + Send;
}

/// Sweep tunables, from `[omdb]`.
#[derive(Debug, Clone, Copy)]
pub struct SweepConfig {
    /// Lookups per UTC day.
    pub daily_limit: u32,
    /// Days before a title is looked up again.
    pub refresh_ttl_days: u32,
}

/// How long the sweep idles once nothing is due.
const IDLE: Duration = Duration::from_secs(6 * 3600);

/// Courtesy gap between lookups.
const SPACING: Duration = Duration::from_millis(250);

/// Titles pulled from the store per batch.
const BATCH: i64 = 100;

/// Spawn the sweep on the tokio runtime. A store error ends only the sweep.
pub fn spawn<S, R>(store: S, source: R, cfg: SweepConfig)
where
    S: RatingsStore + 'static,
    R: RatingsSource + 'static,
{
    tokio::spawn(async move {
        loop {
            let day = unix_now() / 86_400;
            match run_day(&store, &source, cfg, unix_now).await {
                Ok(Day::Drained(n)) => {
                    tracing::info!(looked_up = n, "ratings sweep: nothing due");
                    tokio::time::sleep(IDLE).await;
                }
                Ok(Day::Spent(n)) => {
                    tracing::info!(looked_up = n, "ratings sweep: stopped for today");
                    let next_day = u64::try_from((day + 1) * 86_400 - unix_now()).unwrap_or(0);
                    tokio::time::sleep(Duration::from_secs(next_day)).await;
                }
                Err(error) => {
                    tracing::warn!(%error, "ratings sweep: store error, sweep ended");
                    return;
                }
            }
        }
    });
}

/// How one day's sweep ended, with the number of lookups it made.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Day {
    /// Nothing more is due.
    Drained(u32),
    /// The daily limit was reached or the source said to stop.
    Spent(u32),
}

/// One day's sweep, up to `cfg.daily_limit` lookups. `now` is injectable so
/// the TTL is testable.
pub(crate) async fn run_day<S, R>(
    store: &S,
    source: &R,
    cfg: SweepConfig,
    now: impl Fn() -> i64,
) -> DomainResult<Day>
where
    S: RatingsStore,
    R: RatingsSource,
{
    let ttl = i64::from(cfg.refresh_ttl_days) * 86_400;
    let mut made = 0u32;
    // Titles that failed this pass are skipped for the rest of the day, so a
    // title that always errors can't be pulled again and again.
    let mut failed: std::collections::HashSet<RatingsTarget> = Default::default();
    loop {
        let budget = cfg.daily_limit.saturating_sub(made);
        if budget == 0 {
            return Ok(Day::Spent(made));
        }
        let want = BATCH.min(i64::from(budget)) + i64::try_from(failed.len()).unwrap_or(0);
        let due: Vec<RatingsSubject> = store
            .needing_ratings(want, now() - ttl)
            .await?
            .into_iter()
            .filter(|s| !failed.contains(&s.target))
            .take(usize::try_from(budget).unwrap_or(usize::MAX))
            .collect();
        if due.is_empty() {
            return Ok(Day::Drained(made));
        }
        for subject in due {
            let query = RatingsQuery {
                kind: match subject.target {
                    RatingsTarget::Item(_) => TitleKind::Movie,
                    RatingsTarget::Series(_) => TitleKind::Series,
                },
                imdb_id: subject.imdb_id.as_deref(),
                title: &subject.title,
                year: subject.year,
            };
            made = made.saturating_add(1);
            match source.lookup(&query).await {
                Lookup::Found(ratings) => {
                    store.set_ratings(&subject.target, ratings, now()).await?
                }
                Lookup::NotFound => {
                    store
                        .set_ratings(&subject.target, ExternalRatings::default(), now())
                        .await?
                }
                Lookup::Stop(_) => return Ok(Day::Spent(made)),
                Lookup::Failed(_) => {
                    failed.insert(subject.target.clone());
                }
            }
            tokio::time::sleep(SPACING).await;
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A [`RatingsSource`] answering from a table keyed by IMDb id, else by
    /// title, and recording every query.
    #[derive(Default)]
    pub struct FakeSource {
        pub answers: HashMap<String, Lookup>,
        pub asked: Mutex<Vec<String>>,
    }

    impl RatingsSource for FakeSource {
        async fn lookup(&self, query: &RatingsQuery<'_>) -> Lookup {
            let key = query.imdb_id.unwrap_or(query.title).to_owned();
            if let Ok(mut asked) = self.asked.lock() {
                asked.push(key.clone());
            }
            self.answers.get(&key).cloned().unwrap_or(Lookup::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::fake::FakeSource;
    use super::*;
    use crate::omdb::{OmdbClient, OmdbTransport};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory [`RatingsStore`]: a due list, and what was recorded.
    #[derive(Default)]
    struct Store {
        subjects: Vec<RatingsSubject>,
        set: Mutex<HashMap<RatingsTarget, (ExternalRatings, i64)>>,
    }

    impl RatingsStore for Store {
        async fn needing_ratings(
            &self,
            limit: i64,
            ttl_cutoff: i64,
        ) -> DomainResult<Vec<RatingsSubject>> {
            let set = self.set.lock().unwrap();
            Ok(self
                .subjects
                .iter()
                .filter(|s| set.get(&s.target).is_none_or(|(_, at)| *at < ttl_cutoff))
                .take(usize::try_from(limit).unwrap())
                .cloned()
                .collect())
        }

        async fn set_ratings(
            &self,
            target: &RatingsTarget,
            ratings: ExternalRatings,
            fetched_at: i64,
        ) -> DomainResult<()> {
            self.set
                .lock()
                .unwrap()
                .insert(target.clone(), (ratings, fetched_at));
            Ok(())
        }
    }

    fn film(id: u64, imdb: Option<&str>, title: &str) -> RatingsSubject {
        RatingsSubject {
            target: RatingsTarget::Item(id),
            imdb_id: imdb.map(str::to_owned),
            title: title.into(),
            year: Some(1999),
        }
    }

    const SCORES: ExternalRatings = ExternalRatings {
        imdb: Some(8.7),
        tomatometer: Some(83),
    };

    fn cfg(daily_limit: u32) -> SweepConfig {
        SweepConfig {
            daily_limit,
            refresh_ttl_days: 30,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_sweep_records_found_scores_and_misses_then_drains() {
        let store = Store {
            subjects: vec![film(1, Some("tt1"), "A"), film(2, None, "B")],
            ..Default::default()
        };
        let source = FakeSource {
            answers: HashMap::from([("tt1".to_owned(), Lookup::Found(SCORES))]),
            ..Default::default()
        };
        let day = run_day(&store, &source, cfg(900), || 1_000).await.unwrap();
        assert_eq!(day, Day::Drained(2));
        let set = store.set.lock().unwrap();
        assert_eq!(set[&RatingsTarget::Item(1)], (SCORES, 1_000));
        assert_eq!(
            set[&RatingsTarget::Item(2)],
            (ExternalRatings::default(), 1_000)
        );
        assert_eq!(*source.asked.lock().unwrap(), ["tt1", "B"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_ends_the_day_without_recording_the_title() {
        let store = Store {
            subjects: vec![film(1, Some("tt1"), "A"), film(2, Some("tt2"), "B")],
            ..Default::default()
        };
        let source = FakeSource {
            answers: HashMap::from([(
                "tt1".to_owned(),
                Lookup::Stop("Request limit reached!".into()),
            )]),
            ..Default::default()
        };
        let day = run_day(&store, &source, cfg(900), || 1_000).await.unwrap();
        assert_eq!(day, Day::Spent(1));
        assert!(store.set.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_is_not_recorded_or_retried_the_same_day() {
        let store = Store {
            subjects: vec![film(1, Some("tt1"), "A"), film(2, Some("tt2"), "B")],
            ..Default::default()
        };
        let source = FakeSource {
            answers: HashMap::from([
                ("tt1".to_owned(), Lookup::Failed("http 503".into())),
                ("tt2".to_owned(), Lookup::Found(SCORES)),
            ]),
            ..Default::default()
        };
        let day = run_day(&store, &source, cfg(900), || 1_000).await.unwrap();
        assert_eq!(day, Day::Drained(2));
        assert!(!store
            .set
            .lock()
            .unwrap()
            .contains_key(&RatingsTarget::Item(1)));
        assert_eq!(*source.asked.lock().unwrap(), ["tt1", "tt2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_daily_limit_caps_lookups_and_the_ttl_brings_titles_back() {
        let subjects: Vec<_> = (1..=5).map(|i| film(i, None, &format!("T{i}"))).collect();
        let store = Store {
            subjects,
            ..Default::default()
        };
        let source = FakeSource::default();
        assert_eq!(
            run_day(&store, &source, cfg(3), || 1_000).await.unwrap(),
            Day::Spent(3)
        );
        assert_eq!(
            run_day(&store, &source, cfg(3), || 1_000).await.unwrap(),
            Day::Drained(2)
        );
        assert_eq!(
            run_day(&store, &source, cfg(3), || 1_000).await.unwrap(),
            Day::Drained(0)
        );
        let later = 1_000 + 31 * 86_400;
        assert_eq!(
            run_day(&store, &source, cfg(10), || later).await.unwrap(),
            Day::Drained(5)
        );
    }

    /// The port's contract, run against the fake and against the OMDb
    /// adapter over a recorded body: the same title answers the same way.
    #[tokio::test]
    async fn the_fake_and_omdb_honour_the_same_contract() {
        struct Matrix;
        impl OmdbTransport for Matrix {
            async fn get(&self, params: &[(&str, &str)]) -> (u16, String) {
                let body = if params.contains(&("i", "tt0133093")) {
                    r#"{"Year":"1999","Ratings":[{"Source":"Rotten Tomatoes","Value":"83%"}],"imdbRating":"8.7","Response":"True"}"#
                } else {
                    r#"{"Response":"False","Error":"Movie not found!"}"#
                };
                (200, body.to_owned())
            }
        }
        let fake = FakeSource {
            answers: HashMap::from([("tt0133093".to_owned(), Lookup::Found(SCORES))]),
            ..Default::default()
        };
        let omdb = OmdbClient::with_transport("k".into(), Matrix);
        let hit = RatingsQuery {
            kind: TitleKind::Movie,
            imdb_id: Some("tt0133093"),
            title: "The Matrix",
            year: Some(1999),
        };
        let miss = RatingsQuery {
            imdb_id: Some("tt9999999"),
            ..hit
        };
        for (name, got_hit, got_miss) in [
            ("fake", fake.lookup(&hit).await, fake.lookup(&miss).await),
            ("omdb", omdb.lookup(&hit).await, omdb.lookup(&miss).await),
        ] {
            assert_eq!(got_hit, Lookup::Found(SCORES), "{name}");
            assert_eq!(got_miss, Lookup::NotFound, "{name}");
        }
    }
}
