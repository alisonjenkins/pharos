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

pub mod align;
pub mod cache;
pub mod embedded;
pub mod eta;
pub mod lrc;
pub mod lrclib;
pub mod repick;
pub mod shadow;
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
/// on none of plain ones; `words` holds enhanced-LRC word times when the
/// line has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub start_ms: Option<u64>,
    pub text: String,
    pub words: Vec<WordStamp>,
}

/// An enhanced-LRC word timestamp (`<mm:ss.xx>`) at char offset `at` in its
/// line's text, or at the text's char length when it marks the end of the
/// last word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordStamp {
    pub at: u32,
    pub start_ms: u64,
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

    /// Drop the times of synced lyrics that run past the end of the song.
    ///
    /// Synced lyrics timed for another cut of a song (an album version on a
    /// shorter compilation edit) put lines at the wrong moments, and a line
    /// starting after the song ends shows it. The words are still right, so
    /// they are kept as plain lyrics rather than followed along wrongly.
    #[must_use]
    pub fn fit_to(mut self, duration_ms: Option<u64>) -> Self {
        let Some(duration_ms) = duration_ms.filter(|&d| d > 0) else {
            return self;
        };
        let last = self.lines.iter().filter_map(|l| l.start_ms).max();
        if last.is_some_and(|ms| ms > duration_ms.saturating_add(OVERRUN_SLACK_MS)) {
            self.unsync();
        }
        self
    }

    /// Keep the words as plain lyrics, without line or word times.
    pub fn unsync(&mut self) {
        for line in &mut self.lines {
            line.start_ms = None;
            line.words.clear();
        }
    }
}

/// How far past a song's end a synced line may start before the lyrics are
/// taken to be timed for a different cut: a probed duration and a lyric
/// file's stamps can each be a little off.
const OVERRUN_SLACK_MS: u64 = 3_000;

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
            // A stored item has no `probe.title`: the scanner folds the tag
            // into `item.title`, so that is the title once read back (B363).
            // An untagged song's `item.title` is its filename stem, which is
            // the guess this rule refuses, so the stem is not a title.
            title: tag(&probe.title).or_else(|| {
                let stem = item
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::trim);
                tag(&Some(item.title.clone())).filter(|t| Some(t.as_str()) != stem)
            })?,
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

    /// Every synced record the service holds under the song's title and
    /// artist, in its own order, for picking another when the one found was
    /// not what was sung (B1479). A service that cannot search has none.
    fn search<'a>(
        &'a self,
        _song: &'a Song,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<repick::Candidate>, OnlineError>> + Send + 'a>>
    {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
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
    aligner: Option<Arc<align::Aligner>>,
    /// Songs searched for another lyric since the process started, so a
    /// second ask while one runs, or after one without a disk cache, does
    /// not search again (B1479). Keyed by the song's tags as well, so a
    /// retagged song is searched again. Holds the record picked, apart from
    /// the memo so the memo's emptying cannot lose it: without a disk cache,
    /// or when its write failed, nothing else does.
    repicked: Mutex<HashMap<(u64, String), Option<Lyrics>>>,
    /// Holds the next disk read: says it began, then waits to be let go.
    #[cfg(test)]
    read_disk_paused: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}

type Repicked = Mutex<HashMap<(u64, String), Option<Lyrics>>>;

/// A song's place in [`LyricsFinder::repick`]'s guard, given back when
/// dropped, so a search that fails, waits out a cooldown or is cancelled
/// can be made again; kept once the search's answer is written.
struct Reserved<'a> {
    set: &'a Repicked,
    key: Option<(u64, String)>,
}

impl<'a> Reserved<'a> {
    /// `None` when the song is held already.
    fn take(set: &'a Repicked, key: (u64, String)) -> Option<Self> {
        let first = set
            .lock()
            .map(|mut held| match held.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => false,
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(None);
                    true
                }
            })
            .unwrap_or(false);
        // Lazily: a guard built for a refused take would give back the
        // holder's reservation when dropped.
        first.then(|| Self {
            set,
            key: Some(key),
        })
    }

    fn keep(mut self) {
        self.key = None;
    }

    /// Hold `found` as the song's lyric from now on.
    fn pick(&self, found: &Lyrics) {
        let Some(key) = &self.key else { return };
        if let Ok(mut held) = self.set.lock() {
            held.insert(key.clone(), Some(found.clone()));
        }
    }
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        if let Ok(mut held) = self.set.lock() {
            held.remove(&key);
        }
    }
}

/// An online answer held in memory: what was asked, so a retagged song is
/// asked again, and for a miss when it lapses.
#[derive(Clone)]
struct Remembered {
    key: String,
    found: Option<Lyrics>,
    /// `None` for found words, which stand.
    lapses_at: Option<Instant>,
}

impl LyricsFinder {
    pub fn with_online(online: Arc<dyn OnlineLyrics>) -> Self {
        Self {
            online: Some(online),
            memo: Mutex::default(),
            down_until: Mutex::default(),
            disk: None,
            aligner: None,
            repicked: Mutex::default(),
            #[cfg(test)]
            read_disk_paused: Mutex::default(),
        }
    }

    /// Keep online answers under `dir` as well, so they outlive the process.
    pub fn with_cache(mut self, dir: std::path::PathBuf) -> Self {
        self.disk = Some(Arc::new(cache::DiskCache::new(dir)));
        self
    }

    /// The aligner, when one is configured.
    pub fn aligner(&self) -> Option<Arc<align::Aligner>> {
        self.aligner.clone()
    }

    /// Ask an aligner service for word timestamps when requested.
    pub fn with_aligner(mut self, a: Arc<align::Aligner>) -> Self {
        self.aligner = Some(a);
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
        }
        .map(|l| {
            let last_line_ms = l.lines.iter().filter_map(|line| line.start_ms).max();
            let was_synced = l.synced();
            let fitted = l.fit_to(item.probe.duration_ms);
            if was_synced && !fitted.synced() {
                metrics::counter!(
                    "pharos_lyrics_unsynced_total",
                    "reason" => "past_song_end",
                    "origin" => fitted.origin.label()
                )
                .increment(1);
                tracing::info!(
                    media.id = item.id,
                    origin = fitted.origin.label(),
                    last_line_ms,
                    duration_ms = item.probe.duration_ms,
                    "lyrics: synced lines run past the song's end, shown unsynced"
                );
            }
            fitted
        });
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
        // Taken with the song, before anything is awaited: a repick for a
        // retag made while this lookup waits must count as after it.
        let asked_at = cache::now_secs();
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
        if let Some(picked) = self.picked(item.id, &key) {
            return Some(picked);
        }
        if let Some(known) = self.remembered(item.id, &key) {
            return known;
        }
        if let Some((known, age)) = self.read_disk(item.id, online.name(), &song).await {
            return self
                .remember_unless_repicked(item.id, &key, &known, age)
                .unwrap_or(known);
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
                if let Some(standing) =
                    self.remember_unless_repicked(item.id, &key, &found, Duration::ZERO)
                {
                    return standing;
                }
                self.write_disk(item.id, online.name(), song, found.clone(), asked_at)
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

    /// Another online lyric for `item`, whose lyric `current` the aligner
    /// found was not what was sung (B1479, V1468), scored against what it
    /// `heard`. Searched once per song: the answer, a pick or none, is kept
    /// in place of `current` with the search marked done. `None` when there
    /// is no other, the search was made already, or `current` is local.
    pub async fn repick(
        &self,
        item: &MediaItem,
        current: &Lyrics,
        heard: &align::Heard,
    ) -> Option<Lyrics> {
        let text_recall = heard.recall();
        if current.origin != Origin::Online {
            repick_counted(repick::Outcome::SkippedLocal);
            tracing::warn!(
                media.id = item.id,
                origin = current.origin.label(),
                text_recall,
                heard_words = heard.heard_words,
                "lyrics: not what was sung, and local, so not searched for another"
            );
            return None;
        }
        let online = self.online.as_ref()?;
        let song = Song::of(item)?;
        let Some(reserved) =
            Reserved::take(&self.repicked, (item.id, cache::key(online.name(), &song)))
        else {
            tracing::debug!(
                media.id = item.id,
                "lyrics: another lyric is being searched for already"
            );
            return None;
        };
        if self.repicked_on_disk(item.id, online.name(), &song).await {
            reserved.keep();
            tracing::debug!(
                media.id = item.id,
                "lyrics: another lyric was searched for already"
            );
            return None;
        }
        if self.cooling_down() {
            return None;
        }
        let search = |song: Song| async move {
            match online.search(&song).await {
                Ok(c) => Some(c),
                Err(e) => {
                    if let Ok(mut until) = self.down_until.lock() {
                        *until = Some(Instant::now() + ONLINE_COOLDOWN);
                    }
                    metrics::counter!("pharos_lyrics_online_error_total", "provider" => e.provider)
                        .increment(1);
                    repick_counted(repick::Outcome::Error);
                    tracing::warn!(
                        media.id = item.id,
                        origin = current.origin.label(),
                        text_recall,
                        album = song.album.is_some(),
                        error = %e,
                        "lyrics: not what was sung, and the search for another failed"
                    );
                    None
                }
            }
        };
        let choose = |candidates| {
            item.probe
                .duration_ms
                .and_then(|ms| repick::choose(candidates, current, ms, &heard.text))
        };
        let candidates = search(song.clone()).await?;
        let mut searched = candidates.len();
        let mut pick = choose(candidates);
        // An album tag the provider files differently hides every record;
        // the search is made once more without it.
        let without_album = pick.is_none() && song.album.is_some();
        if without_album {
            let candidates = search(Song {
                album: None,
                ..song.clone()
            })
            .await?;
            searched += candidates.len();
            pick = choose(candidates);
        }
        if let Some(pick) = &pick {
            reserved.pick(&pick.candidate.lyrics);
        }
        // Kept before the write is awaited: the write runs on whether or not
        // this request is still waiting for it, and the search is done.
        reserved.keep();
        let Some(pick) = pick else {
            self.write_repick(item.id, online.name(), song, current.clone(), None)
                .await;
            repick_counted(repick::Outcome::None);
            tracing::warn!(
                media.id = item.id,
                origin = current.origin.label(),
                text_recall,
                searched,
                without_album,
                duration_ms = item.probe.duration_ms,
                "lyrics: not what was sung, and no other record was"
            );
            return None;
        };
        let found = pick.candidate.lyrics;
        self.remember(
            item.id,
            cache::key(online.name(), &song),
            Some(found.clone()),
        );
        self.write_repick(
            item.id,
            online.name(),
            song,
            found.clone(),
            Some(pick.candidate.id),
        )
        .await;
        repick_counted(repick::Outcome::Picked);
        tracing::warn!(
            media.id = item.id,
            origin = current.origin.label(),
            text_recall,
            searched,
            without_album,
            candidate.id = pick.candidate.id,
            candidate.recall = pick.recall,
            "lyrics: not what was sung, another record was and is served instead"
        );
        Some(found.fit_to(item.probe.duration_ms))
    }

    async fn repicked_on_disk(&self, id: u64, provider: &'static str, song: &Song) -> bool {
        let Some(disk) = self.disk.as_ref().map(Arc::clone) else {
            return false;
        };
        let song = song.clone();
        tokio::task::spawn_blocking(move || disk.repicked(id, provider, &song))
            .await
            .unwrap_or(false)
    }

    async fn write_repick(
        &self,
        id: u64,
        provider: &'static str,
        song: Song,
        lyrics: Lyrics,
        picked: Option<u64>,
    ) {
        let Some(disk) = self.disk.as_ref().map(Arc::clone) else {
            return;
        };
        let written = tokio::task::spawn_blocking(move || {
            disk.put_repicked(id, provider, &song, &lyrics, picked);
        })
        .await;
        if let Err(e) = written {
            tracing::warn!(media.id = id, error = %e, "lyrics: cache write task failed");
        }
    }

    fn cooling_down(&self) -> bool {
        self.down_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| Instant::now() < until)
    }

    /// The record a repick of the song as tagged now chose, if one did.
    fn picked(&self, id: u64, key: &str) -> Option<Lyrics> {
        self.repicked
            .lock()
            .ok()?
            .get(&(id, key.to_owned()))
            .cloned()
            .flatten()
    }

    fn remembered(&self, id: u64, key: &str) -> Option<Option<Lyrics>> {
        let r = self.memo.lock().ok()?.get(&id).cloned()?;
        if r.key != key {
            return None;
        }
        // A miss lapses as it does on disk; a long-running server asks again.
        r.lapses_at
            .map_or(true, |at| Instant::now() < at)
            .then_some(r.found)
    }

    /// Remember an answer from the provider or the disk, given `age` ago,
    /// unless a repick of the song was taken while it was read; then the
    /// repick's answer stands and is returned, or `found` when the repick
    /// has not answered yet.
    fn remember_unless_repicked(
        &self,
        id: u64,
        key: &str,
        found: &Option<Lyrics>,
        age: Duration,
    ) -> Option<Option<Lyrics>> {
        // Held across the insert so a repick cannot be taken between the
        // check and the commit.
        let held = self
            .repicked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(picked) = held.get(&(id, key.to_owned())) {
            let picked = picked.clone();
            drop(held);
            tracing::debug!(
                media.id = id,
                "lyrics: lookup answer dropped, the song was repicked meanwhile"
            );
            return Some(
                picked
                    .map(Some)
                    .or_else(|| self.remembered(id, key))
                    .unwrap_or_else(|| found.clone()),
            );
        }
        self.remember_aged(id, key.to_owned(), found.clone(), age);
        drop(held);
        None
    }

    fn remember(&self, id: u64, key: String, found: Option<Lyrics>) {
        self.remember_aged(id, key, found, Duration::ZERO);
    }

    /// Remember an answer given `age` ago, so a miss read back from disk
    /// lapses when it would have on disk, not a whole `MISS_TTL` later.
    fn remember_aged(&self, id: u64, key: String, found: Option<Lyrics>, age: Duration) {
        // A deadline ahead rather than a start behind: `Instant` cannot reach
        // back past the host's boot, and CI hosts are often a day old.
        let lapses_at = found
            .is_none()
            .then(|| Instant::now() + cache::MISS_TTL.saturating_sub(age));
        if let Ok(mut memo) = self.memo.lock() {
            if memo.len() >= ONLINE_MEMO_MAX {
                memo.clear();
            }
            memo.insert(
                id,
                Remembered {
                    key,
                    found,
                    lapses_at,
                },
            );
        }
    }

    /// A standing answer from the disk cache and how old it is, `None` when
    /// there is none.
    async fn read_disk(
        &self,
        id: u64,
        provider: &'static str,
        song: &Song,
    ) -> Option<(Option<Lyrics>, Duration)> {
        #[cfg(test)]
        let paused = self.read_disk_paused.lock().ok().and_then(|mut p| p.take());
        #[cfg(test)]
        if let Some((began, go)) = paused {
            let _ = began.send(());
            let _ = go.await;
        }
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
            cache::Cached::Found(l) => (Some(l), Duration::ZERO),
            cache::Cached::Missing { age } => (None, age),
        })
    }

    async fn write_disk(
        &self,
        id: u64,
        provider: &'static str,
        song: Song,
        found: Option<Lyrics>,
        asked_at: u64,
    ) {
        let Some(disk) = self.disk.as_ref().map(Arc::clone) else {
            return;
        };
        let written = tokio::task::spawn_blocking(move || {
            disk.put_at(id, provider, &song, found.as_ref(), asked_at)
        })
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

fn repick_counted(outcome: repick::Outcome) {
    metrics::counter!(repick::REPICK_TOTAL, "outcome" => outcome.label()).increment(1);
}

/// Word times are still being worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordsPending {
    /// Milliseconds the server expects that to take from now, when it can tell.
    pub eta_ms: Option<u32>,
}

/// Whether karaoke found a song's word times ready when it asked: the
/// share answered `ready` is what aligning the next song ahead raises.
fn count_words_answer(lookup: &align::Lookup) {
    metrics::counter!(WORDS_TOTAL, "answer" => lookup.label()).increment(1);
}

/// Karaoke word-time asks by what they were answered, as
/// [`align::Lookup::label`] names it.
pub const WORDS_TOTAL: &str = "pharos_lyrics_words_total";

/// A signed-in user's view of an item's lyrics. `Ok(None)` means the item has
/// none, or is not a song. When `want_words` is true, word timestamps are
/// filled in from an aligner when ready or flagged as pending.
pub async fn for_item(
    state: &AppState,
    user: &User,
    id: u64,
    want_words: bool,
) -> Result<Option<(Lyrics, Option<WordsPending>)>, DetailError> {
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
    let Some(lyrics) = state.lyrics.find(&item).await else {
        return Ok(None);
    };
    Ok(Some(state.lyrics.served(&item, lyrics, want_words).await))
}

impl LyricsFinder {
    /// `lyrics` of `item` as served: with the aligner's word times when
    /// `want_words`, or another record in its place when it was not sung.
    async fn served(
        &self,
        item: &MediaItem,
        mut lyrics: Lyrics,
        want_words: bool,
    ) -> (Lyrics, Option<WordsPending>) {
        let mut words_pending = None;
        if want_words {
            if let Some(aligner) = self.aligner.clone() {
                let mut lookup = words_for(&aligner, item, &lyrics).await;
                count_words_answer(&lookup);
                if let align::Lookup::Mismatch(heard) = &lookup {
                    // Another record that was sung is aligned in its place, once.
                    if let Some(other) = self.repick(item, &lyrics, heard).await {
                        lyrics = other;
                        lookup = words_for(&aligner, item, &lyrics).await;
                    }
                }
                match lookup {
                    align::Lookup::Ready(aligned) => with_words(&mut lyrics, &aligned),
                    // The lines timed so far, the rest still guessed by the
                    // client until the next answer (ADR-0121).
                    align::Lookup::Partial(eta_ms, aligned, held) => {
                        if held {
                            hold_words(&mut lyrics);
                        }
                        with_words(&mut lyrics, &aligned);
                        words_pending = Some(WordsPending { eta_ms });
                    }
                    align::Lookup::Pending(eta_ms) => {
                        hold_words(&mut lyrics);
                        words_pending = Some(WordsPending { eta_ms });
                    }
                    align::Lookup::Mismatch(_) => unsync_mismatch(&mut lyrics, item.id),
                    align::Lookup::Unavailable => {}
                }
            } else {
                // No aligner configured: the ask still goes unanswered, so it
                // counts against the ready share like the aligner refusing it.
                count_words_answer(&align::Lookup::Unavailable);
            }
        } else if let Some(aligner) = &self.aligner {
            // Lines alone still follow another song, so they go too.
            if mismatched(aligner, item, &lyrics).await {
                unsync_mismatch(&mut lyrics, item.id);
            }
        }
        (lyrics, words_pending)
    }
}

/// A lyric that was not what was sung, with no other record that was, is
/// shown as plain text (B1479, V1468): its times follow another song.
fn unsync_mismatch(lyrics: &mut Lyrics, id: u64) {
    lyrics.unsync();
    metrics::counter!(
        "pharos_lyrics_unsynced_total",
        "reason" => align::LYRICS_MISMATCH,
        "origin" => lyrics.origin.label()
    )
    .increment(1);
    tracing::info!(
        media.id = id,
        origin = lyrics.origin.label(),
        "lyrics: not what was sung, shown unsynced"
    );
}

/// The aligner's answer for `lyrics` of `item`, which stats the file and
/// reads the disk cache, so off the async threads.
async fn words_for(
    aligner: &Arc<align::Aligner>,
    item: &MediaItem,
    lyrics: &Lyrics,
) -> align::Lookup {
    let aligner = Arc::clone(aligner);
    let (item, lyrics) = (item.clone(), lyrics.clone());
    tokio::task::spawn_blocking(move || aligner.words_for(&item, &lyrics))
        .await
        .unwrap_or(align::Lookup::Unavailable)
}

/// Whether the aligner found `lyrics` of `item` not what was sung, read
/// from its cache alone, which stats the file, so off the async threads.
async fn mismatched(aligner: &Arc<align::Aligner>, item: &MediaItem, lyrics: &Lyrics) -> bool {
    let aligner = Arc::clone(aligner);
    let (item, lyrics) = (item.clone(), lyrics.clone());
    tokio::task::spawn_blocking(move || aligner.mismatched(&item, &lyrics).is_some())
        .await
        .unwrap_or(false)
}

/// The lyrics' own word times, held back while the aligner has not yet
/// heard whether they are what was sung (V1468): an online record's
/// enhanced-LRC words may follow another song. The line times stay.
fn hold_words(lyrics: &mut Lyrics) {
    for line in &mut lyrics.lines {
        line.words.clear();
    }
}

/// The aligner's words onto `lyrics`. The aligner moved the song's lines onto
/// the singing (B936) before it found the words, so the lines move with them,
/// enhanced-LRC word times included; a line it gave no words keeps its own.
fn with_words(lyrics: &mut Lyrics, aligned: &align::Aligned) {
    for (line, new_words) in lyrics.lines.iter_mut().zip(aligned.lines.iter()) {
        line.start_ms = line.start_ms.map(|ms| aligned.shift(ms));
        if line.words.is_empty() {
            line.words.clone_from(new_words);
        } else {
            for w in &mut line.words {
                w.start_ms = aligned.shift(w.start_ms);
            }
        }
    }
    start_lines_at_their_first_word(&mut lyrics.lines);
}

/// The aligner may find a line's first word before its stamp (B1165); the
/// player shows a line from its start, so a later start would show that word
/// late. A moved line still starts after the one above it, since the player
/// would skip a line that shares its start with the next.
fn start_lines_at_their_first_word(lines: &mut [LyricLine]) {
    let mut above = None::<u64>;
    for line in lines {
        if let (Some(start), Some(first)) = (line.start_ms, line.words.first()) {
            if first.start_ms < start {
                let lowest = above.map_or(0, |a| a.saturating_add(1));
                line.start_ms = Some(first.start_ms.max(lowest).min(start));
            }
        }
        if line.start_ms.is_some() {
            above = line.start_ms;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn line(start_ms: Option<u64>, text: &str) -> LyricLine {
        LyricLine {
            start_ms,
            text: text.to_owned(),
            words: Vec::new(),
        }
    }

    #[test]
    fn a_line_starts_at_a_first_word_sung_before_its_stamp_but_after_the_line_above() {
        let word = |start_ms| WordStamp { at: 0, start_ms };
        let mut lines = vec![
            LyricLine {
                words: vec![word(9_400)],
                ..line(Some(10_000), "early")
            },
            LyricLine {
                words: vec![word(12_300)],
                ..line(Some(12_000), "late")
            },
            LyricLine {
                words: vec![word(11_000)],
                ..line(Some(13_000), "too early")
            },
            line(Some(14_000), "no words"),
        ];
        start_lines_at_their_first_word(&mut lines);
        let starts: Vec<_> = lines.iter().map(|l| l.start_ms).collect();
        assert_eq!(
            starts,
            [Some(9_400), Some(12_000), Some(12_001), Some(14_000)]
        );
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
    fn synced_lyrics_past_the_end_of_the_song_lose_their_times() {
        // Album-version lyrics (last line at 4:37) on a 2:28 compilation edit.
        let mut late = line(Some(277_190), "(Hey, hey, hey)");
        late.words = vec![WordStamp {
            at: 0,
            start_ms: 277_190,
        }];
        let lyrics = Lyrics::from_lines(
            vec![line(Some(51_680), "I'm the trouble starter"), late],
            Origin::Online,
        )
        .unwrap();
        let fitted = lyrics.fit_to(Some(147_786));
        assert!(!fitted.synced());
        assert!(fitted
            .lines
            .iter()
            .all(|l| l.start_ms.is_none() && l.words.is_empty()));
        assert_eq!(fitted.lines[0].text, "I'm the trouble starter");
    }

    #[test]
    fn synced_lyrics_that_fit_the_song_keep_their_times() {
        let lyrics = Lyrics::from_lines(
            vec![line(Some(0), "a"), line(Some(149_000), "b")],
            Origin::Online,
        )
        .unwrap();
        // Within the slack of a 147.8 s song, and with no duration known.
        assert_eq!(lyrics.clone().fit_to(Some(147_786)), lyrics);
        assert_eq!(lyrics.clone().fit_to(None), lyrics);
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
    async fn a_miss_read_from_disk_lapses_in_memory_when_it_would_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("lyrics");
        let item = song_item(dir.path());
        let song = Song::of(&item).unwrap();
        let six_days = 6 * 24 * 60 * 60;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        cache::DiskCache::new(cache_dir.clone()).put_at(
            item.id,
            "fixed",
            &song,
            None,
            now - six_days,
        );

        let online = Arc::new(Fixed(Ok(None), Default::default()));
        let finder = LyricsFinder::with_online(online.clone()).with_cache(cache_dir);
        assert_eq!(finder.find(&item).await, None);
        assert_eq!(online.1.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The memo keeps it for the day it has left, not seven more.
        let left = finder.memo.lock().unwrap()[&item.id]
            .lapses_at
            .unwrap()
            .saturating_duration_since(Instant::now());
        let day = Duration::from_secs(24 * 60 * 60);
        assert!(
            left <= day && left > day - Duration::from_secs(60),
            "{left:?}"
        );
        finder
            .memo
            .lock()
            .unwrap()
            .get_mut(&item.id)
            .unwrap()
            .lapses_at = Some(Instant::now());
        let key = cache::key("fixed", &song);
        assert_eq!(finder.remembered(item.id, &key), None);
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
    async fn a_song_read_back_from_the_store_is_looked_up_by_its_title() {
        let dir = tempfile::tempdir().unwrap();
        // As the store returns it: the tag's title is the item's own.
        let mut item = song_item(dir.path());
        item.probe.title = None;
        item.title = "Run On".into();
        let song = Song::of(&item).unwrap();
        assert_eq!(
            (song.title.as_str(), song.artist.as_str()),
            ("Run On", "Band")
        );
        item.title = "  ".into();
        assert_eq!(Song::of(&item), None);
        // An untagged song's stored title is its filename stem: not a title.
        item.title = "song".into();
        assert_eq!(Song::of(&item), None);
        // `tag` trims the title, so the stem is trimmed too: `song .mp3`.
        item.path = dir.path().join("song .mp3");
        assert_eq!(Song::of(&item), None);
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

    /// B1415, V1385: each karaoke word-time ask is counted by its answer, so
    /// aligning the next song ahead shows as more `ready` first asks.
    /// Disarm check: with the `counter!` in `count_words_answer` removed,
    /// this fails.
    #[test]
    fn a_word_time_ask_is_counted_by_its_answer() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            count_words_answer(&align::Lookup::Pending(None));
            count_words_answer(&align::Lookup::Pending(Some(5)));
            count_words_answer(&align::Lookup::Unavailable);
        });
        let mut got: Vec<(String, u64)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == WORDS_TOTAL)
            .filter_map(|(k, _, _, v)| match v {
                DebugValue::Counter(n) => Some((
                    k.key()
                        .labels()
                        .find(|l| l.key() == "answer")
                        .map(|l| l.value().to_owned())
                        .unwrap_or_default(),
                    n,
                )),
                _ => None,
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [("pending".to_owned(), 2), ("unavailable".to_owned(), 1)]
        );
    }

    /// A provider whose song search answers `searched`, counting searches.
    struct Searching {
        found: Lyrics,
        searched: Result<Vec<repick::Candidate>, &'static str>,
        searches: std::sync::atomic::AtomicUsize,
        /// The search never answers, as one cut off mid-request.
        stalls: bool,
        /// A search naming the album finds nothing, as a provider that
        /// files the song under another album.
        album_hides: bool,
    }

    impl OnlineLyrics for Searching {
        fn name(&self) -> &'static str {
            "searching"
        }
        fn find<'a>(
            &'a self,
            _song: &'a Song,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Lyrics>, OnlineError>> + Send + 'a>>
        {
            Box::pin(std::future::ready(Ok(Some(self.found.clone()))))
        }
        fn search<'a>(
            &'a self,
            song: &'a Song,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<repick::Candidate>, OnlineError>> + Send + 'a>>
        {
            self.searches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.stalls {
                return Box::pin(std::future::pending());
            }
            if self.album_hides && song.album.is_some() {
                return Box::pin(std::future::ready(Ok(Vec::new())));
            }
            let out = self.searched.clone().map_err(|detail| OnlineError {
                provider: "searching",
                detail: detail.to_owned(),
            });
            Box::pin(std::future::ready(out))
        }
    }

    const SONG_MS: u64 = 200_000;

    fn synced(texts: &[&str], origin: Origin) -> Lyrics {
        Lyrics::from_lines(
            texts
                .iter()
                .enumerate()
                .map(|(i, t)| line(Some(i as u64 * 4_000), t))
                .collect(),
            origin,
        )
        .unwrap()
    }

    fn wrong() -> Lyrics {
        synced(
            &[
                "the lanterns swing in the orchard",
                "the river keeps its time",
            ],
            Origin::Online,
        )
    }

    fn sung() -> Lyrics {
        synced(
            &[
                "grey gulls wheel above the quay",
                "low tide hums along the stones",
            ],
            Origin::Online,
        )
    }

    fn heard() -> align::Heard {
        align::Heard {
            text_recall: Some(0),
            heard_words: 120,
            text: "grey gulls wheel above the quay low tide hums along the stones".into(),
        }
    }

    fn timed_item(dir: &std::path::Path) -> MediaItem {
        let mut item = song_item(dir);
        item.probe.duration_ms = Some(SONG_MS);
        item
    }

    fn searching(searched: Result<Vec<repick::Candidate>, &'static str>) -> Arc<Searching> {
        Arc::new(Searching {
            found: wrong(),
            searched,
            searches: Default::default(),
            stalls: false,
            album_hides: false,
        })
    }

    fn records(lyrics: Lyrics) -> Vec<repick::Candidate> {
        vec![
            repick::Candidate {
                id: 11,
                duration_ms: SONG_MS,
                lyrics: wrong(),
            },
            repick::Candidate {
                id: 12,
                duration_ms: SONG_MS + 1_000,
                lyrics,
            },
        ]
    }

    /// Runs `fut` and returns its output with the repick outcomes counted.
    fn with_outcomes<T>(fut: impl Future<Output = T>) -> (T, Vec<(String, u64)>) {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = metrics::with_local_recorder(&recorder, || rt.block_on(fut));
        let mut got: Vec<(String, u64)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == repick::REPICK_TOTAL)
            .filter_map(|(k, _, _, v)| match v {
                DebugValue::Counter(n) => Some((
                    k.key()
                        .labels()
                        .find(|l| l.key() == "outcome")
                        .map(|l| l.value().to_owned())
                        .unwrap_or_default(),
                    n,
                )),
                _ => None,
            })
            .collect();
        got.sort();
        (out, got)
    }

    fn searches(online: &Searching) -> usize {
        online.searches.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// B1479, V1468: the record that was sung replaces the one that was not,
    /// and is what the song's lyrics are from then on.
    #[test]
    fn a_lyric_not_sung_is_replaced_by_the_record_that_was() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = searching(Ok(records(sung())));
        let finder = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        let ((before, picked, after), counted) = with_outcomes(async {
            let before = finder.find(&item).await.unwrap();
            let picked = finder.repick(&item, &before, &heard()).await;
            (before, picked, finder.find(&item).await)
        });
        assert_eq!(before, wrong());
        assert_eq!(picked, Some(sung()));
        assert_eq!(after, Some(sung()));
        assert_eq!(counted, [("picked".to_owned(), 1)]);
    }

    /// A record filed under another album is found by searching again
    /// without the album.
    #[test]
    fn a_record_filed_under_another_album_is_found_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut item = timed_item(dir.path());
        item.probe.album = Some("Record".into());
        let online = Arc::new(Searching {
            album_hides: true,
            ..Arc::into_inner(searching(Ok(records(sung())))).unwrap()
        });
        let finder = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        let (picked, counted) = with_outcomes(async {
            let before = finder.find(&item).await.unwrap();
            finder.repick(&item, &before, &heard()).await
        });
        assert_eq!(picked, Some(sung()));
        assert_eq!(searches(&online), 2);
        assert_eq!(counted, [("picked".to_owned(), 1)]);
    }

    /// A disk read that lands while a repick of the song is searching is
    /// served, but not remembered over what the repick will choose.
    #[tokio::test]
    async fn a_disk_read_during_a_repick_is_not_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = searching(Ok(records(sung())));
        let warm = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        assert_eq!(warm.find(&item).await, Some(wrong()));
        let finder = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        let song = Song::of(&item).unwrap();
        let key = cache::key(online.name(), &song);
        let searching = Reserved::take(&finder.repicked, (item.id, key.clone())).unwrap();
        assert_eq!(finder.find(&item).await, Some(wrong()));
        assert!(finder.remembered(item.id, &key).is_none());
        drop(searching);
    }

    /// Without a disk cache the record picked is held apart from the memo,
    /// so the memo filling and emptying does not bring the wrong lyric back.
    #[tokio::test]
    async fn the_record_picked_outlives_the_memo_without_a_disk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = searching(Ok(records(sung())));
        let finder = LyricsFinder::with_online(online.clone());
        let before = finder.find(&item).await.unwrap();
        assert_eq!(finder.repick(&item, &before, &heard()).await, Some(sung()));
        for id in 1_000..1_000 + ONLINE_MEMO_MAX as u64 {
            finder.remember(id, "other".into(), None);
        }
        assert_eq!(finder.find(&item).await, Some(sung()));
    }

    /// A request dropped while the search's answer is being written leaves
    /// the song held: the write goes on, and a second search would race it.
    #[tokio::test]
    async fn a_repick_dropped_while_writing_its_answer_stays_held() {
        use std::task::{Context, Waker};
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = searching(Ok(records(sung())));
        let finder = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        let before = finder.find(&item).await.unwrap();
        let heard = heard();
        let mut repick = Box::pin(finder.repick(&item, &before, &heard));
        let mut cx = Context::from_waker(Waker::noop());
        // The search answers at once, so the first wait after it is the
        // write's.
        while searches(&online) == 0 {
            if repick.as_mut().poll(&mut cx).is_ready() {
                panic!("answered before its write was awaited");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(repick);
        let song = Song::of(&item).unwrap();
        let key = (item.id, cache::key(online.name(), &song));
        assert!(finder.repicked.lock().unwrap().contains_key(&key));
    }

    /// A `Searching` whose first lookup notifies `asked` and answers only
    /// once `gate` is notified, as a slow provider response; later lookups
    /// answer at once.
    struct Held {
        inner: Arc<Searching>,
        asked: tokio::sync::Notify,
        gate: tokio::sync::Notify,
        finds: std::sync::atomic::AtomicUsize,
    }

    impl OnlineLyrics for Held {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn find<'a>(
            &'a self,
            song: &'a Song,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Lyrics>, OnlineError>> + Send + 'a>>
        {
            let first = self.finds.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    self.asked.notify_one();
                    self.gate.notified().await;
                }
                self.inner.find(song).await
            })
        }
        fn search<'a>(
            &'a self,
            song: &'a Song,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<repick::Candidate>, OnlineError>> + Send + 'a>>
        {
            self.inner.search(song)
        }
    }

    /// A lookup in flight while another request repicks the song does not
    /// put the record the repick replaced back, in memory or on disk.
    /// Disarm check: with the commit in `online` unguarded, this fails.
    #[test]
    fn a_slow_lookup_does_not_undo_a_repick_made_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let cache = dir.path().join("c");
        let online = Arc::new(Held {
            inner: searching(Ok(records(sung()))),
            asked: tokio::sync::Notify::new(),
            gate: tokio::sync::Notify::new(),
            finds: Default::default(),
        });
        let finder = LyricsFinder::with_online(online.clone()).with_cache(cache.clone());
        let ((slow, picked, after, restarted), _) = with_outcomes(async {
            let (slow, picked) = tokio::join!(finder.find(&item), async {
                online.asked.notified().await;
                let before = finder.find(&item).await?;
                let picked = finder.repick(&item, &before, &heard()).await;
                online.gate.notify_one();
                picked
            });
            let after = finder.find(&item).await;
            let later = LyricsFinder::with_online(online.clone()).with_cache(cache.clone());
            (slow, picked, after, later.find(&item).await)
        });
        assert_eq!(picked, Some(sung()));
        assert_eq!(slow, Some(sung()));
        assert_eq!(after, Some(sung()));
        assert_eq!(restarted, Some(sung()));
    }

    /// V1468's loop guard: a song is searched once, by this finder or by
    /// one started later on the same disk cache.
    /// Disarm check: with the disk cache's `repicked` read skipped in
    /// `repick`, this fails.
    #[test]
    fn a_song_is_searched_for_another_lyric_once() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = searching(Ok(records(sung())));
        let cache = dir.path().join("c");
        let finder = LyricsFinder::with_online(online.clone()).with_cache(cache.clone());
        let ((first, again, restarted), counted) = with_outcomes(async {
            let first = finder.repick(&item, &wrong(), &heard()).await;
            let again = finder.repick(&item, &sung(), &heard()).await;
            let later = LyricsFinder::with_online(online.clone()).with_cache(cache.clone());
            let restarted = later.repick(&item, &sung(), &heard()).await;
            (first, again, restarted)
        });
        assert_eq!(first, Some(sung()));
        assert_eq!((again, restarted), (None, None));
        assert_eq!(searches(&online), 1);
        assert_eq!(counted, [("picked".to_owned(), 1)]);
    }

    /// Disarm check: with the guard keyed by item id alone, the retagged
    /// song is not searched and this fails.
    #[test]
    fn a_retagged_song_is_searched_for_another_lyric_again() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let mut retagged = item.clone();
        retagged.probe.title = Some("Another Song".into());
        let online = searching(Ok(records(sung())));
        let finder = LyricsFinder::with_online(online.clone());
        let ((first, again), _) = with_outcomes(async {
            let first = finder.repick(&item, &wrong(), &heard()).await;
            let again = finder.repick(&retagged, &wrong(), &heard()).await;
            (first, again)
        });
        assert_eq!(first, Some(sung()));
        assert_eq!(again, Some(sung()));
        assert_eq!(searches(&online), 2);
    }

    /// Disarm check: with the guard built eagerly in `take`, the refused
    /// first take gives the reservation back and the second succeeds.
    #[test]
    fn a_held_song_refuses_every_further_take() {
        let set = Mutex::new(HashMap::new());
        let key = (1, "searching/song".to_owned());
        let held = Reserved::take(&set, key.clone());
        assert!(held.is_some());
        assert!(Reserved::take(&set, key.clone()).is_none());
        assert!(Reserved::take(&set, key).is_none());
        drop(held);
    }

    /// Disarm check: with the guard kept when `repick` is dropped, the
    /// second ask finds the song held and does not search.
    #[test]
    fn a_search_cut_off_mid_request_is_made_again() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = Arc::new(Searching {
            found: wrong(),
            searched: Ok(records(sung())),
            searches: Default::default(),
            stalls: true,
            album_hides: false,
        });
        let finder = LyricsFinder::with_online(online.clone());
        let cut_off = std::time::Duration::from_millis(20);
        let (asks, _) = with_outcomes(async {
            let first =
                tokio::time::timeout(cut_off, finder.repick(&item, &wrong(), &heard())).await;
            let again =
                tokio::time::timeout(cut_off, finder.repick(&item, &wrong(), &heard())).await;
            (first.is_err(), again.is_err())
        });
        assert_eq!(asks, (true, true));
        assert_eq!(searches(&online), 2);
    }

    #[test]
    fn with_no_record_sung_the_lyric_stands_and_is_not_searched_again() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let half = synced(
            &[
                "grey gulls wheel above the quay",
                "the river keeps its time",
            ],
            Origin::Online,
        );
        let online = searching(Ok(records(half)));
        let cache = dir.path().join("c");
        let finder = LyricsFinder::with_online(online.clone()).with_cache(cache.clone());
        let ((picked, restarted, kept), counted) = with_outcomes(async {
            let picked = finder.repick(&item, &wrong(), &heard()).await;
            let later = LyricsFinder::with_online(online.clone()).with_cache(cache.clone());
            let restarted = later.repick(&item, &wrong(), &heard()).await;
            (picked, restarted, later.find(&item).await)
        });
        assert_eq!((picked, restarted), (None, None));
        assert_eq!(kept, Some(wrong()));
        assert_eq!(searches(&online), 1);
        assert_eq!(counted, [("none".to_owned(), 1)]);
    }

    /// A lookup that took the song as tagged, then waited on the disk cache
    /// while the item was retagged and repicked, does not replace the
    /// repick when it answers: it was asked before the repick, though it
    /// reached the provider after. Disarm check: with `asked_at` taken
    /// after `read_disk`, this fails.
    #[test]
    fn a_lookup_paused_across_a_retag_and_repick_does_not_replace_it() {
        use tokio::sync::oneshot;
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let mut retagged = item.clone();
        retagged.probe.title = Some("Another Song".into());
        let online = searching(Ok(records(sung())));
        let finder = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        let (began_tx, began) = oneshot::channel();
        let (go, go_rx) = oneshot::channel();
        *finder.read_disk_paused.lock().unwrap() = Some((began_tx, go_rx));
        let next_second = || async {
            let now = cache::now_secs();
            while cache::now_secs() == now {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let ((_, picked), _) = with_outcomes(async {
            tokio::join!(finder.online(&item), async {
                began.await.unwrap();
                next_second().await;
                let picked = finder.repick(&retagged, &wrong(), &heard()).await;
                next_second().await;
                go.send(()).unwrap();
                picked
            })
        });
        assert_eq!(picked, Some(sung().fit_to(Some(SONG_MS))));
        let later = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        let song = Song::of(&retagged).unwrap();
        assert!(later
            .disk
            .as_ref()
            .unwrap()
            .repicked(retagged.id, online.name(), &song));
    }

    #[test]
    fn a_local_lyric_is_not_searched_for_another() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = searching(Ok(records(sung())));
        let finder = LyricsFinder::with_online(online.clone());
        let local = synced(&["the lanterns swing in the orchard"], Origin::SidecarLrc);
        let (picked, counted) = with_outcomes(finder.repick(&item, &local, &heard()));
        assert_eq!(picked, None);
        assert_eq!(searches(&online), 0);
        assert_eq!(counted, [("skipped_local".to_owned(), 1)]);
    }

    #[test]
    fn a_failed_search_is_made_again_once_the_provider_cools_down() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        let online = searching(Err("503"));
        let finder = LyricsFinder::with_online(online.clone()).with_cache(dir.path().join("c"));
        let ((failed, cooling, retried), counted) = with_outcomes(async {
            let failed = finder.repick(&item, &wrong(), &heard()).await;
            let cooling = finder.repick(&item, &wrong(), &heard()).await;
            *finder.down_until.lock().unwrap() = Some(Instant::now());
            let retried = finder.repick(&item, &wrong(), &heard()).await;
            (failed, cooling, retried)
        });
        assert_eq!((failed, cooling, retried), (None, None, None));
        assert_eq!(searches(&online), 2);
        assert_eq!(counted, [("error".to_owned(), 2)]);
    }

    /// V1468: a lyric found not to be what was sung is never served synced,
    /// and each time it is served so is counted. Disarm check: with
    /// `unsync()` removed from `unsync_mismatch`, this fails.
    /// V1468 on an ask for lines alone: the lines of a lyric found not
    /// sung follow another song too, so are not served with times; another
    /// lyric's keep theirs. Disarm check: with the lines-only branch of
    /// `served` removed, this fails.
    #[test]
    fn a_lines_only_ask_for_a_lyric_not_sung_is_served_unsynced() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        std::fs::write(&item.path, b"audio").unwrap();
        let aligner = align::tests::found_not_sung(dir.path(), &item, &wrong());
        let finder = LyricsFinder::default().with_aligner(aligner);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (not_sung, pending) = rt.block_on(finder.served(&item, wrong(), false));
        assert_eq!(pending, None);
        assert!(not_sung.lines.iter().all(|l| l.start_ms.is_none()));
        let (other, _) = rt.block_on(finder.served(&item, sung(), false));
        assert_eq!(other, sung());
    }

    /// V1468: an online lyric with words on some lines (partly enhanced
    /// LRC) is aligned, and its own words may follow another song as much
    /// as the aligner's, so they are held back with them, pending and
    /// partial alike, and come back once the aligner clears the text.
    /// Disarm check: with `hold_words` emptied, this fails.
    #[test]
    fn a_lyrics_own_words_are_held_until_its_text_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let item = timed_item(dir.path());
        std::fs::write(&item.path, b"audio").unwrap();
        let mut lyrics = synced(
            &[
                "grey gulls wheel above the quay",
                "the river keeps its time",
            ],
            Origin::Online,
        );
        lyrics.lines[0].words = vec![WordStamp {
            at: 0,
            start_ms: 200,
        }];
        let (aligner, key) = align::tests::in_flight(dir.path(), &item, &lyrics);
        let finder = LyricsFinder::default().with_aligner(Arc::clone(&aligner));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let serve = || rt.block_on(finder.served(&item, lyrics.clone(), true));
        let words = |l: &Lyrics| l.lines.iter().map(|l| l.words.len()).collect::<Vec<_>>();

        let (pending, waiting) = serve();
        assert!(waiting.is_some());
        assert_eq!(words(&pending), [0, 0], "pending");
        assert_eq!(pending.lines[0].start_ms, Some(0));

        let send = |e: &str| align::tests::send(&aligner, &item, &key, e, 2);
        send(r#"{"event":"offset","offset_ms":0,"aligner_version":6,"lines":2}"#);
        send(r#"{"event":"line","index":1,"words":[{"at":0,"start_ms":4100}],"score":null}"#);
        let (partial, waiting) = serve();
        assert!(waiting.is_some());
        assert_eq!(words(&partial), [0, 0], "partial before text_clear");

        send(r#"{"event":"text_clear"}"#);
        let (cleared, _) = serve();
        assert_eq!(words(&cleared), [1, 1], "after text_clear");
        assert_eq!(cleared.lines[0].words[0].start_ms, 200);
    }

    #[test]
    fn a_lyric_not_sung_is_served_unsynced_and_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let mut lyrics = wrong();
        lyrics.lines[0].words = vec![WordStamp {
            at: 0,
            start_ms: 10,
        }];
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || unsync_mismatch(&mut lyrics, 1));
        assert!(lyrics
            .lines
            .iter()
            .all(|l| l.start_ms.is_none() && l.words.is_empty()));
        let got: Vec<(Vec<(String, String)>, u64)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_lyrics_unsynced_total")
            .filter_map(|(k, _, _, v)| match v {
                DebugValue::Counter(n) => Some((
                    k.key()
                        .labels()
                        .map(|l| (l.key().to_owned(), l.value().to_owned()))
                        .collect(),
                    n,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            [(
                vec![
                    ("reason".to_owned(), "lyrics_mismatch".to_owned()),
                    ("origin".to_owned(), "online".to_owned()),
                ],
                1
            )]
        );
    }
}
