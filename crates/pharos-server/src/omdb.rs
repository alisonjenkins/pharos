//! B341 — OMDb (omdbapi.com) adapter for [`RatingsSource`]: a title's IMDb
//! score and Rotten Tomatoes Tomatometer.
//!
//! One `GET` per lookup: by IMDb id (`i=`) when the title has one, else by
//! title (`t=`), type and year. OMDb answers HTTP 200 for a miss too, with
//! `"Response":"False"` and an `Error` string, so the outcome is read from the
//! body: a miss is [`Lookup::NotFound`], a spent daily quota or a bad key is
//! [`Lookup::Stop`] (nothing else will succeed today), anything else is
//! [`Lookup::Failed`] and is retried on a later pass. A value OMDb doesn't
//! have is the string `"N/A"`.
//!
//! The HTTP call sits behind [`OmdbTransport`], like `tvdb.rs`, so the parsing
//! and outcome mapping are tested against recorded bodies with no network.

use std::future::Future;

use pharos_core::ExternalRatings;

use crate::ratings::{Lookup, RatingsQuery, RatingsSource, TitleKind};

const API_BASE: &str = "https://www.omdbapi.com/";

/// The HTTP call an OMDb lookup rides on.
pub trait OmdbTransport: Send + Sync {
    /// `GET {API_BASE}?{params}`, as `(status, body)`. A transport failure
    /// (no connection, timeout) is status `0` with an empty body.
    fn get(&self, params: &[(&str, &str)]) -> impl Future<Output = (u16, String)> + Send;
}

/// The real [`OmdbTransport`], over `reqwest`.
#[derive(Clone)]
pub struct ReqwestTransport {
    http: reqwest::Client,
}

/// Long enough for a slow answer, short enough that a hung connection can't
/// stall the sweep.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl OmdbTransport for ReqwestTransport {
    async fn get(&self, params: &[(&str, &str)]) -> (u16, String) {
        let Ok(resp) = self.http.get(API_BASE).query(params).send().await else {
            return (0, String::new());
        };
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        (status, body)
    }
}

/// An OMDb client over its [`OmdbTransport`].
#[derive(Clone)]
pub struct OmdbClient<T: OmdbTransport> {
    api_key: String,
    transport: T,
}

impl OmdbClient<ReqwestTransport> {
    pub fn new(api_key: String) -> Self {
        Self::with_transport(api_key, ReqwestTransport::new())
    }
}

impl<T: OmdbTransport> OmdbClient<T> {
    pub fn with_transport(api_key: String, transport: T) -> Self {
        Self { api_key, transport }
    }
}

impl<T: OmdbTransport> RatingsSource for OmdbClient<T> {
    async fn lookup(&self, query: &RatingsQuery<'_>) -> Lookup {
        let kind = match query.kind {
            TitleKind::Movie => "movie",
            TitleKind::Series => "series",
        };
        let year = query.year.map(|y| y.to_string());
        let mut params = vec![("apikey", self.api_key.as_str()), ("type", kind)];
        match query.imdb_id {
            Some(id) => params.push(("i", id)),
            None => {
                params.push(("t", query.title));
                if let Some(year) = year.as_deref() {
                    params.push(("y", year));
                }
            }
        }
        let (status, body) = self.transport.get(&params).await;
        let lookup = outcome(status, &body, query);
        if let Lookup::Stop(reason) | Lookup::Failed(reason) = &lookup {
            tracing::warn!(
                status,
                title = query.title,
                imdb_id = query.imdb_id.unwrap_or_default(),
                reason = reason.as_str(),
                "omdb: lookup failed"
            );
        }
        lookup
    }
}

/// What an OMDb answer means for the sweep.
fn outcome(status: u16, body: &str, query: &RatingsQuery<'_>) -> Lookup {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Lookup::Failed(format!("http {status}, body is not JSON"));
    };
    if field(&v, "Response") != Some("True") {
        let error = field(&v, "Error").unwrap_or("no error given");
        return match error {
            "Movie not found!" | "Series not found!" | "Incorrect IMDb ID." => Lookup::NotFound,
            e if e.contains("limit") || e.contains("API key") || e.contains("activated") => {
                Lookup::Stop(e.to_owned())
            }
            e => Lookup::Failed(format!("http {status}: {e}")),
        };
    }
    // A title search can land on a same-name title from another year; only
    // the year the library names is the right one.
    if query.imdb_id.is_none() {
        if let Some(want) = query.year {
            let got = field(&v, "Year").and_then(first_year);
            if got != Some(want) {
                return Lookup::NotFound;
            }
        }
    }
    Lookup::Found(ExternalRatings {
        imdb: field(&v, "imdbRating").and_then(|r| r.parse::<f32>().ok()),
        tomatometer: v
            .get("Ratings")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
            .find(|r| field(r, "Source") == Some("Rotten Tomatoes"))
            .and_then(|r| field(r, "Value"))
            .and_then(|p| p.strip_suffix('%'))
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p <= 100),
    })
}

fn field<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|f| f.as_str()).filter(|s| *s != "N/A")
}

/// The first year of OMDb's `Year`: `"1999"`, or a run such as `"2008–2013"`
/// or `"2019–"`.
fn first_year(year: &str) -> Option<u32> {
    year.get(..4)?.parse().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::Mutex;

    const MATRIX: &str = r#"{"Title":"The Matrix","Year":"1999","Rated":"R","Ratings":[{"Source":"Internet Movie Database","Value":"8.7/10"},{"Source":"Rotten Tomatoes","Value":"83%"},{"Source":"Metacritic","Value":"73/100"}],"Metascore":"73","imdbRating":"8.7","imdbVotes":"2,195,334","imdbID":"tt0133093","Type":"movie","Response":"True"}"#;
    const BREAKING_BAD: &str = r#"{"Title":"Breaking Bad","Year":"2008–2013","Ratings":[{"Source":"Internet Movie Database","Value":"9.5/10"}],"imdbRating":"9.5","imdbID":"tt0903747","Type":"series","totalSeasons":"5","Response":"True"}"#;
    const NO_SCORES: &str = r#"{"Title":"Obscure","Year":"2001","Ratings":[],"imdbRating":"N/A","imdbID":"tt0000001","Type":"movie","Response":"True"}"#;
    const NOT_FOUND: &str = r#"{"Response":"False","Error":"Movie not found!"}"#;
    const LIMIT: &str = r#"{"Response":"False","Error":"Request limit reached!"}"#;
    const BAD_KEY: &str = r#"{"Response":"False","Error":"Invalid API key!"}"#;

    /// Answers every call with one recorded body and keeps the params sent.
    struct Recorded {
        status: u16,
        body: &'static str,
        sent: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl Recorded {
        fn new(status: u16, body: &'static str) -> Self {
            Self {
                status,
                body,
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl OmdbTransport for Recorded {
        async fn get(&self, params: &[(&str, &str)]) -> (u16, String) {
            self.sent.lock().unwrap().push(
                params
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            );
            (self.status, self.body.to_owned())
        }
    }

    fn client(status: u16, body: &'static str) -> OmdbClient<Recorded> {
        OmdbClient::with_transport("k".into(), Recorded::new(status, body))
    }

    fn by_id(id: &str) -> RatingsQuery<'_> {
        RatingsQuery {
            kind: TitleKind::Movie,
            imdb_id: Some(id),
            title: "The Matrix",
            year: Some(1999),
        }
    }

    fn by_title(kind: TitleKind, title: &str, year: Option<u32>) -> RatingsQuery<'_> {
        RatingsQuery {
            kind,
            imdb_id: None,
            title,
            year,
        }
    }

    #[tokio::test]
    async fn an_imdb_id_lookup_reads_both_scores() {
        let c = client(200, MATRIX);
        let got = c.lookup(&by_id("tt0133093")).await;
        assert_eq!(
            got,
            Lookup::Found(ExternalRatings {
                imdb: Some(8.7),
                tomatometer: Some(83),
            })
        );
        let sent = c.transport.sent.lock().unwrap();
        assert!(sent[0].contains(&("i".into(), "tt0133093".into())));
        assert!(sent[0].contains(&("type".into(), "movie".into())));
        assert!(!sent[0].iter().any(|(k, _)| k == "t"));
    }

    #[tokio::test]
    async fn a_title_lookup_sends_title_type_and_year_and_takes_a_run_of_years() {
        let c = client(200, BREAKING_BAD);
        let got = c
            .lookup(&by_title(TitleKind::Series, "Breaking Bad", Some(2008)))
            .await;
        assert_eq!(
            got,
            Lookup::Found(ExternalRatings {
                imdb: Some(9.5),
                tomatometer: None,
            })
        );
        let sent = c.transport.sent.lock().unwrap();
        for p in [("t", "Breaking Bad"), ("y", "2008"), ("type", "series")] {
            assert!(sent[0].contains(&(p.0.into(), p.1.into())), "{p:?}");
        }
    }

    #[tokio::test]
    async fn a_title_match_from_another_year_is_not_found() {
        let c = client(200, BREAKING_BAD);
        let got = c
            .lookup(&by_title(TitleKind::Series, "Breaking Bad", Some(2021)))
            .await;
        assert_eq!(got, Lookup::NotFound);
    }

    #[tokio::test]
    async fn n_a_scores_are_absent_not_zero() {
        let got = client(200, NO_SCORES).lookup(&by_id("tt0000001")).await;
        assert_eq!(got, Lookup::Found(ExternalRatings::default()));
    }

    #[tokio::test]
    async fn a_miss_is_not_found_and_a_spent_quota_or_bad_key_stops_the_sweep() {
        assert_eq!(
            client(200, NOT_FOUND).lookup(&by_id("tt1")).await,
            Lookup::NotFound
        );
        assert!(matches!(
            client(401, LIMIT).lookup(&by_id("tt1")).await,
            Lookup::Stop(_)
        ));
        assert!(matches!(
            client(401, BAD_KEY).lookup(&by_id("tt1")).await,
            Lookup::Stop(_)
        ));
    }

    #[tokio::test]
    async fn a_dropped_connection_or_garbage_fails_without_a_verdict() {
        assert!(matches!(
            client(0, "").lookup(&by_id("tt1")).await,
            Lookup::Failed(_)
        ));
        assert!(matches!(
            client(503, "<html>busy</html>").lookup(&by_id("tt1")).await,
            Lookup::Failed(_)
        ));
    }

    #[tokio::test]
    async fn a_tomatometer_over_100_is_dropped() {
        const ODD: &str = r#"{"Year":"1999","Ratings":[{"Source":"Rotten Tomatoes","Value":"130%"}],"imdbRating":"7.0","Response":"True"}"#;
        assert_eq!(
            client(200, ODD).lookup(&by_id("tt1")).await,
            Lookup::Found(ExternalRatings {
                imdb: Some(7.0),
                tomatometer: None,
            })
        );
    }
}
