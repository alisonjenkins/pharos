//! Online lyrics kept on disk (B361/V301), so a song is asked of the provider
//! once rather than once per server start. One small JSON file per item under
//! the cache directory: words are kept until the song's tags change, a miss
//! is asked again after [`MISS_TTL`] because the provider's database grows.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{LyricLine, Lyrics, Origin, Song, WordStamp};

/// How long a provider's "no lyrics for this song" stands before it is asked
/// again.
pub const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A provider's answer as it was cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cached {
    Found(Lyrics),
    /// The provider had none, `age` ago.
    Missing {
        age: Duration,
    },
}

pub struct DiskCache {
    dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Record {
    /// The provider and song asked about; a retagged file asks again.
    key: String,
    /// Unix seconds when the provider answered.
    fetched_at: u64,
    lines: Option<Vec<Line>>,
}

#[derive(Serialize, Deserialize)]
struct Line {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_ms: Option<u64>,
    text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    words: Vec<Word>,
}

#[derive(Serialize, Deserialize)]
struct Word {
    at: u32,
    start_ms: u64,
}

impl DiskCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// The cached answer for `id` asked of `provider` about `song`, or `None`
    /// when there is none still standing. Blocking.
    pub fn get(&self, id: u64, provider: &str, song: &Song) -> Option<Cached> {
        let path = self.path(id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "lyrics: cache read failed");
                return None;
            }
        };
        let record: Record = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "lyrics: cache entry unreadable");
                return None;
            }
        };
        if record.key != key(provider, song) {
            return None;
        }
        match record.lines {
            Some(lines) => Lyrics::from_lines(
                lines
                    .into_iter()
                    .map(|l| LyricLine {
                        start_ms: l.start_ms,
                        text: l.text,
                        words: l
                            .words
                            .into_iter()
                            .map(|w| WordStamp {
                                at: w.at,
                                start_ms: w.start_ms,
                            })
                            .collect(),
                    })
                    .collect(),
                Origin::Online,
            )
            .map(Cached::Found),
            None => {
                let age = now_secs().saturating_sub(record.fetched_at);
                (age < MISS_TTL.as_secs()).then_some(Cached::Missing {
                    age: Duration::from_secs(age),
                })
            }
        }
    }

    /// Keep the provider's answer. Blocking; a failure is logged and the
    /// answer is only lost, never wrong.
    pub fn put(&self, id: u64, provider: &str, song: &Song, found: Option<&Lyrics>) {
        self.put_at(id, provider, song, found, now_secs());
    }

    pub(super) fn put_at(
        &self,
        id: u64,
        provider: &str,
        song: &Song,
        found: Option<&Lyrics>,
        at: u64,
    ) {
        let record = Record {
            key: key(provider, song),
            fetched_at: at,
            lines: found.map(|l| {
                l.lines
                    .iter()
                    .map(|l| Line {
                        start_ms: l.start_ms,
                        text: l.text.clone(),
                        words: l
                            .words
                            .iter()
                            .map(|w| Word {
                                at: w.at,
                                start_ms: w.start_ms,
                            })
                            .collect(),
                    })
                    .collect()
            }),
        };
        if let Err(e) = write(&self.dir, &self.path(id), &record) {
            tracing::warn!(media.id = id, dir = %self.dir.display(), error = %e, "lyrics: cache write failed");
        }
    }
}

/// Written beside its final name and renamed over it, so a reader never sees
/// half a file.
fn write(dir: &Path, path: &Path, record: &Record) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, &bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// What an answer was asked about; the memo and the disk compare it alike.
/// A JSON array, so a tag holding the separator cannot make two different
/// songs read as one (a title "A\nB" by "C" is not "A" by "B\nC").
pub(super) fn key(provider: &str, song: &Song) -> String {
    serde_json::json!([
        provider,
        song.title,
        song.artist,
        song.album.as_deref().unwrap_or_default(),
        song.duration_ms.map_or(0, |ms| ms / 1000)
    ])
    .to_string()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn song() -> Song {
        Song {
            title: "Song".into(),
            artist: "Band".into(),
            album: Some("Album".into()),
            duration_ms: Some(200_500),
        }
    }

    fn words() -> Lyrics {
        Lyrics::from_lines(
            vec![
                LyricLine {
                    start_ms: Some(1_000),
                    text: "One".into(),
                    words: vec![
                        WordStamp {
                            at: 0,
                            start_ms: 1_000,
                        },
                        WordStamp {
                            at: 3,
                            start_ms: 1_400,
                        },
                    ],
                },
                LyricLine {
                    start_ms: Some(2_000),
                    text: String::new(),
                    words: Vec::new(),
                },
            ],
            Origin::Online,
        )
        .unwrap()
    }

    #[test]
    fn found_lyrics_come_back_as_they_went_in() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().join("lyrics"));
        assert_eq!(cache.get(7, "lrclib", &song()), None);
        cache.put(7, "lrclib", &song(), Some(&words()));
        assert_eq!(
            cache.get(7, "lrclib", &song()),
            Some(Cached::Found(words()))
        );
        // Another item's entry is its own.
        assert_eq!(cache.get(8, "lrclib", &song()), None);
    }

    #[test]
    fn a_miss_stands_until_it_is_a_week_old() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf());
        cache.put(7, "lrclib", &song(), None);
        assert!(matches!(
            cache.get(7, "lrclib", &song()),
            Some(Cached::Missing { age }) if age < Duration::from_secs(5)
        ));
        let old = now_secs() - MISS_TTL.as_secs();
        cache.put_at(7, "lrclib", &song(), None, old);
        assert_eq!(cache.get(7, "lrclib", &song()), None);
        // Found words do not age out.
        cache.put_at(7, "lrclib", &song(), Some(&words()), 0);
        assert!(matches!(
            cache.get(7, "lrclib", &song()),
            Some(Cached::Found(_))
        ));
    }

    #[test]
    fn a_retagged_song_or_another_provider_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf());
        cache.put(7, "lrclib", &song(), Some(&words()));
        let mut retagged = song();
        retagged.title = "Song (Live)".into();
        assert_eq!(cache.get(7, "lrclib", &retagged), None);
        assert_eq!(cache.get(7, "other", &song()), None);
    }

    #[test]
    fn a_line_break_moved_between_tags_is_a_retag() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf());
        let mut before = song();
        before.title = "Song\nGuest".into();
        before.artist = "Band".into();
        cache.put(7, "lrclib", &before, Some(&words()));
        let mut after = song();
        after.title = "Song".into();
        after.artist = "Guest\nBand".into();
        assert_ne!(key("lrclib", &before), key("lrclib", &after));
        assert_eq!(cache.get(7, "lrclib", &after), None);
    }

    #[test]
    fn a_damaged_entry_is_a_miss_of_the_cache_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("7.json"), b"{not json").unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf());
        assert_eq!(cache.get(7, "lrclib", &song()), None);
    }

    #[test]
    fn an_old_entry_without_words_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let song = song();
        let raw = serde_json::json!({
            "key": key("lrclib", &song),
            "fetched_at": 1_700_000_000u64,
            "lines": [{ "start_ms": 1_000u64, "text": "One" }],
        });
        std::fs::write(dir.path().join("7.json"), raw.to_string()).unwrap();
        let cache = DiskCache::new(dir.path().to_path_buf());
        let expected = Lyrics::from_lines(
            vec![LyricLine {
                start_ms: Some(1_000),
                text: "One".into(),
                words: Vec::new(),
            }],
            Origin::Online,
        )
        .unwrap();
        assert_eq!(cache.get(7, "lrclib", &song), Some(Cached::Found(expected)));
    }
}
