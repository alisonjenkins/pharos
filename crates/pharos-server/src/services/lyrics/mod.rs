//! Lyrics for a song (013 US7), served to the native player as
//! `Library.lyrics` and to Jellyfin clients as `GET /Audio/{id}/Lyrics`.
//!
//! Sources are tried in a fixed order and the first that has words wins
//! (B360/V300):
//!
//! 1. an `.lrc` sidecar beside the file (the listener put it there);
//! 2. the file's own tags: an ID3v2 `SYLT` frame, else `USLT` / `LYRICS` /
//!    `©lyr`, whose text is itself LRC more often than not;
//! 3. a `.txt` sidecar;
//! 4. an online provider, only when the operator turned one on.
//!
//! Local sources are read on every request so an edited sidecar shows on the
//! next play. Online answers, hits and misses alike, are remembered per item,
//! in memory and, when a cache directory is set, on disk (B361/V301).

pub mod cache;
pub mod embedded;
pub mod lrc;
pub mod lrclib;
pub mod sidecar;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pharos_core::{DomainError, MediaItem, MediaKind, MediaStore as _, User};

use crate::services::library::DetailError;
use crate::state::AppState;

/// One line of lyrics. `start_ms` is set on every line of synced lyrics and
/// on none of plain ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub start_ms: Option<u64>,
    pub text: String,
}

/// Where a song's lyrics came from. `label()` is a metric label value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    SidecarLrc,
    EmbeddedSynced,
    EmbeddedText,
    SidecarText,
    Online,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::SidecarLrc => "sidecar_lrc",
            Origin::EmbeddedSynced => "embedded_synced",
            Origin::EmbeddedText => "embedded_text",
            Origin::SidecarText => "sidecar_text",
            Origin::Online => "online",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub origin: Origin,
}

impl Lyrics {
    /// Wrap parsed lines, or `None` when there are no words to show.
    pub fn from_lines(lines: Vec<LyricLine>, origin: Origin) -> Option<Self> {
        lines
            .iter()
            .any(|l| !l.text.trim().is_empty())
            .then_some(Self { lines, origin })
    }

    /// Every line carries a start time, so a player can follow along.
    pub fn synced(&self) -> bool {
        !self.lines.is_empty() && self.lines.iter().all(|l| l.start_ms.is_some())
    }
}

/// What an online provider is asked about a song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Song {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
}

impl Song {
    /// The song a provider could look up, or `None` when the item lacks a
    /// title or artist tag; guessing from the filename finds the wrong song.
    pub fn of(item: &MediaItem) -> Option<Self> {
        let tag = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let probe = &item.probe;
        Some(Self {
            title: tag(&probe.title)?,
            artist: tag(&probe.artist).or_else(|| tag(&probe.album_artist))?,
            album: tag(&probe.album),
            duration_ms: probe.duration_ms,
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{provider}: {detail}")]
pub struct OnlineError {
    pub provider: &'static str,
    pub detail: String,
}

/// A remote lyrics service. `Ok(None)` is a clean miss.
pub trait OnlineLyrics: Send + Sync {
    fn name(&self) -> &'static str;
    fn find<'a>(
        &'a self,
        song: &'a Song,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Lyrics>, OnlineError>> + Send + 'a>>;
}

/// Online answers kept before the memo is emptied and starts again; bounds
/// memory on a large library without an LRU's bookkeeping.
const ONLINE_MEMO_MAX: usize = 4096;

/// After an online lookup fails, the provider is skipped this long, so an
/// outage costs one timeout rather than one per song played.
const ONLINE_COOLDOWN: Duration = Duration::from_secs(60);

/// Finds a song's lyrics across the sources in the module order.
#[derive(Default)]
pub struct LyricsFinder {
    online: Option<Arc<dyn OnlineLyrics>>,
    memo: Mutex<HashMap<u64, Remembered>>,
    down_until: Mutex<Option<Instant>>,
    disk: Option<Arc<cache::DiskCache>>,
}

/// An online answer held in memory: what was asked, so a retagged song is
/// asked again, and when, so a miss can lapse.
#[derive(Clone)]
struct Remembered {
    key: String,
    found: Option<Lyrics>,
    at: Instant,
}

impl LyricsFinder {
    pub fn with_online(online: Arc<dyn OnlineLyrics>) -> Self {
        Self {
            online: Some(online),
            memo: Mutex::default(),
            down_until: Mutex::default(),
            disk: None,
        }
    }

    /// Keep online answers under `dir` as well, so they outlive the process.
    pub fn with_cache(mut self, dir: std::path::PathBuf) -> Self {
        self.disk = Some(Arc::new(cache::DiskCache::new(dir)));
        self
    }

    pub async fn find(&self, item: &MediaItem) -> Option<Lyrics> {
        let started = Instant::now();
        let path = item.path.clone();
        let local = tokio::task::spawn_blocking(move || local(&path))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "lyrics: local read task failed");
                None
            });
        let found = match local {
            Some(found) => Some(found),
            None => self.online(item).await,
        };
        let outcome = found.as_ref().map_or("none", |l| l.origin.label());
        metrics::counter!("pharos_lyrics_lookup_total", "outcome" => outcome).increment(1);
        tracing::debug!(
            media.id = item.id,
            outcome,
            synced = found.as_ref().is_some_and(Lyrics::synced),
            lines = found.as_ref().map_or(0, |l| l.lines.len()),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "lyrics lookup"
        );
        found
    }

    async fn online(&self, item: &MediaItem) -> Option<Lyrics> {
        let online = self.online.as_ref()?;
        let Some(song) = Song::of(item) else {
            tracing::debug!(
                media.id = item.id,
                provider = online.name(),
                "lyrics: online skipped, no title and artist tags"
            );
            return None;
        };
        let key = cache::key(online.name(), &song);
        if let Some(known) = self.remembered(item.id, &key) {
            return known;
        }
        if let Some(known) = self.read_disk(item.id, online.name(), &song).await {
            self.remember(item.id, key, known.clone());
            return known;
        }
        if self.cooling_down() {
            tracing::debug!(
                media.id = item.id,
                provider = online.name(),
                "lyrics: online skipped, provider failed recently"
            );
            return None;
        }
        match online.find(&song).await {
            Ok(found) => {
                self.remember(item.id, key, found.clone());
                self.write_disk(item.id, online.name(), song, found.clone())
                    .await;
                found
            }
            Err(e) => {
                // Not remembered: a provider outage should not hide lyrics
                // until the server restarts.
                if let Ok(mut until) = self.down_until.lock() {
                    *until = Some(Instant::now() + ONLINE_COOLDOWN);
                }
                metrics::counter!("pharos_lyrics_online_error_total", "provider" => e.provider)
                    .increment(1);
                tracing::warn!(
                    media.id = item.id,
                    title = %song.title,
                    artist = %song.artist,
                    error = %e,
                    "lyrics: online lookup failed"
                );
                None
            }
        }
    }

    fn cooling_down(&self) -> bool {
        self.down_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| Instant::now() < until)
    }

    fn remembered(&self, id: u64, key: &str) -> Option<Option<Lyrics>> {
        let r = self.memo.lock().ok()?.get(&id).cloned()?;
        if r.key != key {
            return None;
        }
        // A miss lapses as it does on disk; a long-running server asks again.
        (r.found.is_some() || r.at.elapsed() < cache::MISS_TTL).then_some(r.found)
    }

    fn remember(&self, id: u64, key: String, found: Option<Lyrics>) {
        if let Ok(mut memo) = self.memo.lock() {
            if memo.len() >= ONLINE_MEMO_MAX {
                memo.clear();
            }
            memo.insert(
                id,
                Remembered {
                    key,
                    found,
                    at: Instant::now(),
                },
            );
        }
    }

    /// A standing answer from the disk cache, `None` when there is none.
    async fn read_disk(
        &self,
        id: u64,
        provider: &'static str,
        song: &Song,
    ) -> Option<Option<Lyrics>> {
        let disk = Arc::clone(self.disk.as_ref()?);
        let song = song.clone();
        let cached = tokio::task::spawn_blocking(move || disk.get(id, provider, &song))
            .await
            .ok()
            .flatten()?;
        tracing::debug!(
            media.id = id,
            provider,
            "lyrics: online answer from the disk cache"
        );
        Some(match cached {
            cache::Cached::Found(l) => Some(l),
            cache::Cached::Missing => None,
        })
    }

    async fn write_disk(&self, id: u64, provider: &'static str, song: Song, found: Option<Lyrics>) {
        let Some(disk) = self.disk.as_ref().map(Arc::clone) else {
            return;
        };
        let written =
            tokio::task::spawn_blocking(move || disk.put(id, provider, &song, found.as_ref()))
                .await;
        if let Err(e) = written {
            tracing::warn!(media.id = id, error = %e, "lyrics: cache write task failed");
        }
    }
}

/// The local sources, in order. Blocking: reads the file and its sidecars.
fn local(path: &std::path::Path) -> Option<Lyrics> {
    sidecar::lrc(path)
        .or_else(|| embedded::read(path))
        .or_else(|| sidecar::text(path))
}

/// A signed-in user's view of an item's lyrics. `Ok(None)` means the item has
/// none, or is not a song.
pub async fn for_item(
    state: &AppState,
    user: &User,
    id: u64,
) -> Result<Option<Lyrics>, DetailError> {
    crate::services::access::check_item(state, user, id, "item")
        .await
        .map_err(DetailError::Hidden)?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        DomainError::NotFound(_) => DetailError::NotFound,
        other => DetailError::Store(other),
    })?;
    if item.kind != MediaKind::Audio {
        return Ok(None);
    }
    Ok(state.lyrics.find(&item).await)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn line(start_ms: Option<u64>, text: &str) -> LyricLine {
        LyricLine {
            start_ms,
            text: text.to_owned(),
        }
    }

    #[test]
    fn blank_lines_alone_are_no_lyrics() {
        let blank = vec![line(Some(0), ""), line(Some(5), "  ")];
        assert_eq!(Lyrics::from_lines(blank, Origin::SidecarLrc), None);
        assert_eq!(Lyrics::from_lines(Vec::new(), Origin::SidecarLrc), None);
    }

    #[test]
    fn synced_needs_a_start_on_every_line() {
        let all = Lyrics::from_lines(vec![line(Some(0), "a"), line(Some(5), "b")], Origin::Online);
        assert!(all.unwrap().synced());
        let some = Lyrics::from_lines(vec![line(Some(0), "a"), line(None, "b")], Origin::Online);
        assert!(!some.unwrap().synced());
    }

    #[test]
    fn origin_labels_are_distinct() {
        let all = [
            Origin::SidecarLrc,
            Origin::EmbeddedSynced,
            Origin::EmbeddedText,
            Origin::SidecarText,
            Origin::Online,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), all.len());
    }

    struct Fixed(
        Result<Option<Lyrics>, &'static str>,
        std::sync::atomic::AtomicUsize,
    );

    impl OnlineLyrics for Fixed {
        fn name(&self) -> &'static str {
            "fixed"
        }
        fn find<'a>(
            &'a self,
            _song: &'a Song,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Lyrics>, OnlineError>> + Send + 'a>>
        {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = self.0.clone().map_err(|detail| OnlineError {
                provider: "fixed",
                detail: detail.to_owned(),
            });
            Box::pin(async move { out })
        }
    }

    fn song_item(dir: &std::path::Path) -> MediaItem {
        MediaItem {
            id: 1,
            path: dir.join("song.mp3"),
            kind: MediaKind::Audio,
            probe: pharos_core::MediaProbe {
                title: Some("Song".into()),
                artist: Some("Band".into()),
                ..pharos_core::MediaProbe::default()
            },
            ..MediaItem::default()
        }
    }

    #[tokio::test]
    async fn a_sidecar_wins_over_the_online_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("song.lrc"), "[00:01.00]local\n").unwrap();
        let online = Arc::new(Fixed(
            Ok(Lyrics::from_lines(
                vec![line(None, "remote")],
                Origin::Online,
            )),
            Default::default(),
        ));
        let finder = LyricsFinder::with_online(online.clone());
        let found = finder.find(&song_item(dir.path())).await.unwrap();
        assert_eq!(found.origin, Origin::SidecarLrc);
        assert_eq!(online.1.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_online_miss_is_remembered_but_an_error_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let item = song_item(dir.path());

        let miss = Arc::new(Fixed(Ok(None), Default::default()));
        let finder = LyricsFinder::with_online(miss.clone());
        assert_eq!(finder.find(&item).await, None);
        assert_eq!(finder.find(&item).await, None);
        assert_eq!(miss.1.load(std::sync::atomic::Ordering::SeqCst), 1);

        let down = Arc::new(Fixed(Err("503"), Default::default()));
        let finder = LyricsFinder::with_online(down.clone());
        assert_eq!(finder.find(&item).await, None);
        // Within the cooldown the provider is not asked again.
        assert_eq!(finder.find(&item).await, None);
        assert_eq!(down.1.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Once it passes, the same song is looked up again: the error was
        // not remembered as a miss.
        *finder.down_until.lock().unwrap() = Some(Instant::now());
        assert_eq!(finder.find(&item).await, None);
        assert_eq!(down.1.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn online_answers_outlive_the_finder_through_the_disk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("lyrics");
        let item = song_item(dir.path());
        let words = Lyrics::from_lines(vec![line(Some(0), "remote")], Origin::Online);

        let first = Arc::new(Fixed(Ok(words.clone()), Default::default()));
        let finder = LyricsFinder::with_online(first.clone()).with_cache(cache.clone());
        assert_eq!(finder.find(&item).await, words);
        assert_eq!(first.1.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A restarted server answers from disk without asking the provider.
        let second = Arc::new(Fixed(Ok(None), Default::default()));
        let finder = LyricsFinder::with_online(second.clone()).with_cache(cache.clone());
        assert_eq!(finder.find(&item).await, words);
        assert_eq!(second.1.load(std::sync::atomic::Ordering::SeqCst), 0);

        // A miss is kept too, so it is not asked again on every play.
        let mut other = item.clone();
        other.id = 2;
        let miss = Arc::new(Fixed(Ok(None), Default::default()));
        let finder = LyricsFinder::with_online(miss.clone()).with_cache(cache.clone());
        assert_eq!(finder.find(&other).await, None);
        let finder = LyricsFinder::with_online(miss.clone()).with_cache(cache);
        assert_eq!(finder.find(&other).await, None);
        assert_eq!(miss.1.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_retagged_song_is_asked_again_rather_than_served_from_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut item = song_item(dir.path());
        let words = Lyrics::from_lines(vec![line(Some(0), "remote")], Origin::Online);
        let online = Arc::new(Fixed(Ok(words), Default::default()));
        let finder = LyricsFinder::with_online(online.clone());
        finder.find(&item).await.unwrap();
        finder.find(&item).await.unwrap();
        assert_eq!(online.1.load(std::sync::atomic::Ordering::SeqCst), 1);
        item.probe.title = Some("Another Song".into());
        finder.find(&item).await.unwrap();
        assert_eq!(online.1.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_song_without_an_artist_tag_is_not_looked_up_online() {
        let dir = tempfile::tempdir().unwrap();
        let mut item = song_item(dir.path());
        item.probe.artist = None;
        let online = Arc::new(Fixed(Ok(None), Default::default()));
        let finder = LyricsFinder::with_online(online.clone());
        assert_eq!(finder.find(&item).await, None);
        assert_eq!(online.1.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
