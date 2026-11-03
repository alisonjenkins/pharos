//! LRCLIB (lrclib.net), a free lyrics database with no API key: the online
//! adapter behind [`OnlineLyrics`]. Off unless `[lyrics].online` is set,
//! because it sends each played song's title and artist to a third party.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;

use super::{lrc, Lyrics, OnlineError, OnlineLyrics, Origin, Song};

const BASE: &str = "https://lrclib.net/api";
const NAME: &str = "lrclib";

/// LRCLIB answers from a database lookup; a reply slower than this is an
/// outage, and the listener is waiting on it.
const TIMEOUT: Duration = Duration::from_secs(8);

pub struct LrcLib {
    http: reqwest::Client,
    base: String,
}

impl LrcLib {
    pub fn new() -> Self {
        Self::with_base(BASE)
    }

    pub fn with_base(base: &str) -> Self {
        // LRCLIB asks clients to name themselves in the User-Agent.
        let ua = format!(
            "pharos/{} (https://github.com/alisonjenkins/pharos)",
            env!("CARGO_PKG_VERSION")
        );
        let http = reqwest::Client::builder()
            .user_agent(ua)
            .timeout(TIMEOUT)
            .build()
            // Only TLS backend init fails here; V17 forbids `expect`.
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    async fn get(&self, song: &Song) -> Result<Option<Lyrics>, OnlineError> {
        let fail = |detail: String| OnlineError {
            provider: NAME,
            detail,
        };
        let mut query: Vec<(&str, String)> = vec![
            ("track_name", song.title.clone()),
            ("artist_name", song.artist.clone()),
        ];
        if let Some(album) = &song.album {
            query.push(("album_name", album.clone()));
        }
        if let Some(ms) = song.duration_ms {
            query.push(("duration", (ms / 1000).to_string()));
        }
        let reply = self
            .http
            .get(format!("{}/get", self.base))
            .query(&query)
            .send()
            .await
            .map_err(|e| fail(format!("request: {e}")))?;
        let status = reply.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(fail(format!("status {status}")));
        }
        let record: Record = reply.json().await.map_err(|e| fail(format!("body: {e}")))?;
        Ok(record.lyrics())
    }
}

impl Default for LrcLib {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default)]
    synced_lyrics: Option<String>,
    #[serde(default)]
    plain_lyrics: Option<String>,
}

impl Record {
    fn lyrics(&self) -> Option<Lyrics> {
        [&self.synced_lyrics, &self.plain_lyrics]
            .into_iter()
            .flatten()
            .find_map(|text| Lyrics::from_lines(lrc::parse(text), Origin::Online))
    }
}

impl OnlineLyrics for LrcLib {
    fn name(&self) -> &'static str {
        NAME
    }

    fn find<'a>(
        &'a self,
        song: &'a Song,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Lyrics>, OnlineError>> + Send + 'a>> {
        Box::pin(self.get(song))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn synced_lyrics_win_over_plain_and_null_fields_are_a_miss() {
        let both: Record = serde_json::from_str(
            r#"{"syncedLyrics":"[00:01.00]Sync","plainLyrics":"Plain","instrumental":false}"#,
        )
        .unwrap();
        let found = both.lyrics().unwrap();
        assert!(found.synced());
        assert_eq!(found.origin, Origin::Online);

        let plain: Record =
            serde_json::from_str(r#"{"syncedLyrics":null,"plainLyrics":"Plain"}"#).unwrap();
        assert!(!plain.lyrics().unwrap().synced());

        let none: Record =
            serde_json::from_str(r#"{"syncedLyrics":null,"plainLyrics":null,"instrumental":true}"#)
                .unwrap();
        assert_eq!(none.lyrics(), None);
    }
}
