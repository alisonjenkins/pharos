//! LRCLIB (lrclib.net), a free lyrics database with no API key: the online
//! adapter behind [`OnlineLyrics`]. Off unless `[lyrics].online` is set,
//! because it sends each played song's title and artist to a third party.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;

use super::repick::Candidate;
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
        let http = crate::http_client::builder()
            .user_agent(ua)
            .timeout(TIMEOUT)
            .build()
            // Only TLS backend init fails here; V17 forbids `expect`.
            .unwrap_or_else(|_| crate::http_client::client());
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
        let mut query = song_query(song);
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

    async fn search(&self, song: &Song) -> Result<Vec<Candidate>, OnlineError> {
        let fail = |detail: String| OnlineError {
            provider: NAME,
            detail,
        };
        let reply = self
            .http
            .get(format!("{}/search", self.base))
            .query(&song_query(song))
            .send()
            .await
            .map_err(|e| fail(format!("search request: {e}")))?;
        let status = reply.status();
        if !status.is_success() {
            return Err(fail(format!("search status {status}")));
        }
        let found: Vec<Found> = reply
            .json()
            .await
            .map_err(|e| fail(format!("search body: {e}")))?;
        Ok(found.into_iter().filter_map(Found::candidate).collect())
    }
}

/// The song's title, artist and, when tagged, album, as LRCLIB's `get` and
/// `search` both take them.
fn song_query(song: &Song) -> Vec<(&'static str, String)> {
    let mut query = vec![
        ("track_name", song.title.clone()),
        ("artist_name", song.artist.clone()),
    ];
    if let Some(album) = &song.album {
        query.push(("album_name", album.clone()));
    }
    query
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

/// One `/search` result: a record with its id and length in seconds.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Found {
    id: u64,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    synced_lyrics: Option<String>,
}

impl Found {
    /// A record that can be aligned: synced lines and a length to match.
    fn candidate(self) -> Option<Candidate> {
        let duration_ms = self
            .duration
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| (d * 1000.0).round() as u64)?;
        let lyrics = Lyrics::from_lines(lrc::parse(self.synced_lyrics.as_deref()?), Origin::Online)
            .filter(Lyrics::synced)?;
        Some(Candidate {
            id: self.id,
            duration_ms,
            lyrics,
        })
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

    fn search<'a>(
        &'a self,
        song: &'a Song,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Candidate>, OnlineError>> + Send + 'a>> {
        Box::pin(LrcLib::search(self, song))
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

    #[test]
    fn a_search_keeps_records_with_synced_lines_and_a_length() {
        let found: Vec<Found> = serde_json::from_str(
            r#"[
                {"id": 11, "duration": 201.4, "syncedLyrics": "[00:01.00]grey gulls wheel", "plainLyrics": "grey gulls wheel"},
                {"id": 12, "duration": 201.0, "syncedLyrics": null, "plainLyrics": "plain only"},
                {"id": 13, "duration": null, "syncedLyrics": "[00:01.00]no length"},
                {"id": 14, "duration": 199.0, "syncedLyrics": "", "instrumental": true}
            ]"#,
        )
        .unwrap();
        let got: Vec<Candidate> = found.into_iter().filter_map(Found::candidate).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 11);
        assert_eq!(got[0].duration_ms, 201_400);
        assert!(got[0].lyrics.synced());
    }

    /// Answers one request with an empty search result and hands back its
    /// request line.
    async fn one_search() -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                assert!(n > 0, "client closed before the end of the request head");
                got.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&got);
            let _ = tx.send(head.lines().next().unwrap_or_default().to_owned());
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
                )
                .await;
        });
        (base, rx)
    }

    #[tokio::test]
    async fn a_search_names_the_album_when_the_song_has_one() {
        let mut song = Song {
            title: "Song".into(),
            artist: "Band".into(),
            album: Some("Record".into()),
            duration_ms: Some(200_000),
        };
        let (base, asked) = one_search().await;
        LrcLib::with_base(&base).search(&song).await.unwrap();
        let line = asked.await.unwrap();
        assert!(line.contains("album_name=Record"), "{line}");

        song.album = None;
        let (base, asked) = one_search().await;
        LrcLib::with_base(&base).search(&song).await.unwrap();
        let line = asked.await.unwrap();
        assert!(!line.contains("album_name"), "{line}");
    }
}
