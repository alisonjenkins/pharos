//! The music play queue (B435/V375): what plays after the current song.
//!
//! The queue is one ordered list. Playing a song from outside it seeds the
//! rest of that song's album behind whatever the listener added; "Play next"
//! puts a song at the front and "Add to queue" at the very end. Starting a
//! song that is in the queue drops everything up to and including it, so the
//! queue always reads as "what comes next". Shuffle reorders it and keeps the
//! order from before, which turning shuffle off brings back.
//!
//! It lives for the tab, in the app's context, so it survives moving between
//! pages and from song to song, and is kept in the tab's session storage so
//! it survives a reload as the playing song does (FR-704).
//!
//! Films, episodes and music videos have a queue of their own (B906/V853),
//! the same [`Queue`] held as [`VideoQueueCtx`]: only what the viewer added,
//! played after the video on screen and before its next episode.

use std::collections::{HashMap, HashSet, VecDeque};

use dioxus::prelude::*;
use pharos_client::api::{Item, ItemKind, ItemRef};
use serde::{Deserialize, Serialize};

use super::views::{display_name, episode_label};

/// How a song got into the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The listener asked for it.
    Added,
    /// It follows the playing song on its album.
    Album,
    /// A generated mix put it there.
    Mix,
}

impl Source {
    /// The `data-source` on its queue row.
    pub fn label(self) -> &'static str {
        match self {
            Source::Added => "added",
            Source::Album => "album",
            Source::Mix => "mix",
        }
    }
}

/// One song in the queue. `key` is unique for the queue's lifetime, so the
/// same song queued twice is two rows that drag and remove apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: u64,
    pub id: ItemRef,
    pub name: String,
    pub runtime_ms: u64,
    pub source: Source,
}

/// A song as the queue needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: ItemRef,
    pub name: String,
    pub runtime_ms: u64,
}

impl Track {
    /// A video as its queue row names it: an episode with its show and
    /// number, so a queue of several shows reads unambiguously.
    pub fn video(item: &Item) -> Self {
        let title = display_name(item);
        let name = match (item.series_name.as_deref(), episode_label(item)) {
            (Some(show), Some(number)) => format!("{show} · {number} · {title}"),
            _ => title,
        };
        Track {
            id: item.id,
            name,
            runtime_ms: item.runtime_ms,
        }
    }
}

/// Whether the video queue takes `kind`: a film (music videos are stored
/// as films) or an episode.
pub fn video_kind(kind: ItemKind) -> bool {
    matches!(kind, ItemKind::Movie | ItemKind::Episode)
}

/// Where a video's next comes from (V853), for its log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextFrom {
    /// The video queue's first that is not the video playing.
    Queue,
    /// The next episode: nothing is queued.
    Episode,
    /// A group decides what plays; the queue is not consulted.
    Group,
    /// Nothing follows.
    Nothing,
}

impl NextFrom {
    pub fn label(self) -> &'static str {
        match self {
            NextFrom::Queue => "queue",
            NextFrom::Episode => "episode",
            NextFrom::Group => "group",
            NextFrom::Nothing => "nothing",
        }
    }
}

/// What plays after a video: the queued one, else the next episode; in a
/// group only the next episode, which the group moves to.
pub fn video_next(
    queued: Option<ItemRef>,
    episode: Option<ItemRef>,
    grouped: bool,
) -> (Option<ItemRef>, NextFrom) {
    match (grouped, queued, episode) {
        (true, _, next) => (next, NextFrom::Group),
        (false, Some(q), _) => (Some(q), NextFrom::Queue),
        (false, None, Some(e)) => (Some(e), NextFrom::Episode),
        (false, None, None) => (None, NextFrom::Nothing),
    }
}

/// The log line for a video's next, with its inputs and the verdict.
pub fn video_next_line(
    item: ItemRef,
    queued: Option<ItemRef>,
    episode: Option<ItemRef>,
    grouped: bool,
) -> String {
    let (next, from) = video_next(queued, episode, grouped);
    let show = |r: Option<ItemRef>| r.map_or_else(|| "none".to_owned(), |r| r.to_string());
    format!(
        "pharos: video next item={item} next={} from={} queued={} episode={} grouped={grouped}",
        show(next),
        from.label(),
        show(queued),
        show(episode)
    )
}

impl From<&Item> for Track {
    fn from(item: &Item) -> Self {
        Track {
            id: item.id,
            name: item.name.clone(),
            runtime_ms: item.runtime_ms,
        }
    }
}

/// What the listener asked the queue for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueueAction {
    /// Play this entry now, skipping those before it.
    Play(u64),
    Remove(u64),
    /// Move the entry to this position.
    Move {
        key: u64,
        to: usize,
    },
    Shuffle(bool),
    Clear,
}

impl QueueAction {
    /// The `action` its log line carries.
    pub fn label(self) -> &'static str {
        match self {
            QueueAction::Play(_) => "play",
            QueueAction::Remove(_) => "remove",
            QueueAction::Move { .. } => "move",
            QueueAction::Shuffle(true) => "shuffle_on",
            QueueAction::Shuffle(false) => "shuffle_off",
            QueueAction::Clear => "clear",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Queue {
    upcoming: Vec<Entry>,
    shuffled: bool,
    /// The order, by key, from before shuffle went on.
    unshuffled: Vec<u64>,
    /// The songs started, newest first, at most `RECENT`.
    recent: VecDeque<ItemRef>,
    /// The songs Previous walks back through, newest (the playing song)
    /// first, each with how it got into the queue. Unlike `recent`, a step
    /// back takes songs off it.
    behind: VecDeque<(ItemRef, Source)>,
    /// The song a step back went to, which starts without reseeding the
    /// album part of the queue.
    returning: Option<(ItemRef, Source)>,
    /// The song a step back went to, which plays from its start rather than
    /// from where it was left.
    from_start: Option<ItemRef>,
    /// The song the queue was last told started: once it knows the playing
    /// song, the queue alone says what is next.
    current: Option<ItemRef>,
    next_key: u64,
    /// Read back after a reload: the playing song starting again is the
    /// same song resuming, not a new start that reseeds its album.
    restored: bool,
    /// The signed-in user the queue was built for, once known: a reload
    /// keeps it, and another user's queue is dropped rather than shown.
    owner: Option<String>,
}

/// The tab's session-storage key the music queue is kept under.
pub const QUEUE_KEY: &str = "pharos.queue";

/// The saved form's version; a record of another version is not read.
const SAVED_VERSION: u32 = 1;

/// What a reload keeps of a [`Queue`]. Item ids are their URL form.
#[derive(Serialize, Deserialize)]
struct Saved {
    v: u32,
    upcoming: Vec<SavedEntry>,
    shuffled: bool,
    unshuffled: Vec<u64>,
    recent: Vec<String>,
    behind: Vec<(String, Source)>,
    current: Option<String>,
    next_key: u64,
    /// [`Queue::owner`]: a record with songs and no owner is not read.
    #[serde(default)]
    user: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedEntry {
    key: u64,
    id: String,
    name: String,
    runtime_ms: u64,
    source: Source,
}

/// Why a saved queue was not read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    /// Not JSON of the saved shape: the reason.
    Malformed(String),
    /// Written by another version of the app.
    Version(u32),
    /// An item id that does not parse.
    BadId(String),
    /// Songs saved before any user was known: whose they are is unknown.
    Unbound,
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreError::Malformed(e) => write!(f, "malformed: {e}"),
            RestoreError::Version(v) => write!(f, "version {v}, expected {SAVED_VERSION}"),
            RestoreError::BadId(id) => write!(f, "bad item id {id:?}"),
            RestoreError::Unbound => f.write_str("no signed-in user recorded"),
        }
    }
}

/// How many started songs the queue remembers.
const RECENT: usize = 50;

/// How far into a song Previous starts it again rather than going back: the
/// usual music player rule, long enough for a quick double press to go back.
pub const RESTART_AFTER_MS: u64 = 3_000;

/// The app-wide queue, provided by the app's root.
#[derive(Clone, Copy)]
pub struct QueueCtx(pub Signal<Queue>);

/// The app-wide video queue (B906/V853), provided by the app's root, and
/// whether its panel is open in the player, kept from video to video.
#[derive(Clone, Copy)]
pub struct VideoQueueCtx(pub Signal<Queue>, pub Signal<bool>);

impl Queue {
    pub fn upcoming(&self) -> &[Entry] {
        &self.upcoming
    }

    pub fn shuffled(&self) -> bool {
        self.shuffled
    }

    /// The queue's first song.
    pub fn next(&self) -> Option<ItemRef> {
        self.upcoming.first().map(|e| e.id)
    }

    /// The first entry that is not `playing`: a video queued again while
    /// it plays waits for a later turn rather than following itself.
    pub fn next_besides(&self, playing: ItemRef) -> Option<ItemRef> {
        self.upcoming.iter().map(|e| e.id).find(|id| *id != playing)
    }

    /// A video started: its first entry leaves the queue, wherever it is,
    /// so a queued film played from its page is not played again. Returns
    /// whether one did.
    pub fn take(&mut self, id: ItemRef) -> bool {
        let Some(at) = self.upcoming.iter().position(|e| e.id == id) else {
            return false;
        };
        self.upcoming.remove(at);
        self.forget_gone();
        true
    }

    /// What plays after `playing`: the queue's first once the queue knows
    /// `playing` started (so a cleared queue stays cleared), else the queue's
    /// first or `album_next`, the next track on `playing`'s album.
    pub fn next_after(&self, playing: ItemRef, album_next: Option<ItemRef>) -> Option<ItemRef> {
        if self.current == Some(playing) {
            self.next()
        } else {
            self.next().or(album_next)
        }
    }

    /// The songs started lately, newest first.
    pub fn recent(&self) -> impl Iterator<Item = ItemRef> + '_ {
        self.recent.iter().copied()
    }

    /// Whether `id` starting comes from the queue: its first song, or the
    /// song a step back went to.
    pub fn expects(&self, id: ItemRef) -> bool {
        self.next() == Some(id) || self.returning.is_some_and(|(r, _)| r == id)
    }

    /// Whether a step back is on its way: the song it went to has not
    /// started yet.
    pub fn stepping_back(&self) -> bool {
        self.returning.is_some()
    }

    /// Whether `id` plays from 0 s, as the song a step back went to does,
    /// whatever position the server saved for it.
    pub fn plays_from_start(&self, id: ItemRef) -> bool {
        self.from_start == Some(id)
    }

    pub fn contains(&self, id: ItemRef) -> bool {
        self.upcoming.iter().any(|e| e.id == id)
    }

    fn entry(&mut self, track: Track, source: Source) -> Entry {
        self.next_key += 1;
        Entry {
            key: self.next_key,
            id: track.id,
            name: track.name,
            runtime_ms: track.runtime_ms,
            source,
        }
    }

    /// "Play next": to the front, in the order given, so a whole album
    /// plays next from its first song.
    pub fn play_next(&mut self, tracks: Vec<Track>) {
        let entries: Vec<Entry> = tracks
            .into_iter()
            .map(|t| self.entry(t, Source::Added))
            .collect();
        if self.shuffled {
            self.unshuffled.splice(0..0, entries.iter().map(|e| e.key));
        }
        self.upcoming.splice(0..0, entries);
    }

    /// "Add to queue": to the very end.
    pub fn add(&mut self, tracks: Vec<Track>) {
        self.append(tracks, Source::Added);
    }

    /// To the very end, marked with `source`.
    pub fn append(&mut self, tracks: Vec<Track>, source: Source) {
        for track in tracks {
            let entry = self.entry(track, source);
            if self.shuffled {
                self.unshuffled.push(entry.key);
            }
            self.upcoming.push(entry);
        }
    }

    /// Take out every entry `source` put in.
    pub fn drop_source(&mut self, source: Source) {
        self.upcoming.retain(|e| e.source != source);
        self.forget_gone();
    }

    /// `id` started playing. The queue's first song comes out: songs leave
    /// the queue from the front, as Next, a song ending and picking an entry
    /// (which drops those before it) all play the first. Any other song
    /// starts a new album, so the album part of the queue becomes the songs
    /// after it on its album, `album_after`; what the listener or a mix
    /// added stays. `album_after` of `None` (its album could not be read)
    /// leaves the album part alone.
    pub fn started(&mut self, id: ItemRef, album_after: Option<&[Item]>, seed: u64) {
        // After a reload the playing song starts again: the queue already
        // holds what follows it, as the listener left it.
        if std::mem::take(&mut self.restored) && self.current == Some(id) {
            return;
        }
        if self.recent.front() != Some(&id) {
            self.recent.push_front(id);
            self.recent.truncate(RECENT);
        }
        self.current = Some(id);
        if self.from_start != Some(id) {
            self.from_start = None;
        }
        // The song stepped back to comes first: a queued copy of it that
        // reached the front meanwhile is a separate entry and stays queued.
        if let Some((_, source)) = self.returning.take().filter(|(r, _)| *r == id) {
            self.behind_push(id, source);
            return;
        }
        if self.next() == Some(id) {
            let source = self.upcoming.remove(0).source;
            self.behind_push(id, source);
            self.forget_gone();
            return;
        }
        // A song started outside the queue heads the album part after it.
        self.behind_push(id, Source::Album);
        let Some(album_after) = album_after else {
            return;
        };
        self.upcoming.retain(|e| e.source != Source::Album);
        let mut album: Vec<Entry> = album_after
            .iter()
            // An album folder can hold a clip; the music queue plays songs.
            .filter(|item| item.kind == ItemKind::Audio)
            .map(|item| self.entry(Track::from(item), Source::Album))
            .collect();
        if self.shuffled {
            self.unshuffled.extend(album.iter().map(|e| e.key));
            shuffle(&mut album, seed);
        }
        self.upcoming.extend(album);
        self.forget_gone();
    }

    pub fn apply(&mut self, action: QueueAction, seed: u64) {
        match action {
            QueueAction::Play(key) => {
                // Starting the song takes it out, as it is then first.
                if let Some(at) = self.position(key) {
                    self.upcoming.drain(..at);
                }
            }
            QueueAction::Remove(key) => self.upcoming.retain(|e| e.key != key),
            QueueAction::Move { key, to } => {
                if let Some(from) = self.position(key) {
                    let entry = self.upcoming.remove(from);
                    let to = to.min(self.upcoming.len());
                    self.upcoming.insert(to, entry);
                }
            }
            QueueAction::Shuffle(true) if !self.shuffled => {
                self.shuffled = true;
                self.unshuffled = self.upcoming.iter().map(|e| e.key).collect();
                shuffle(&mut self.upcoming, seed);
            }
            QueueAction::Shuffle(false) if self.shuffled => {
                self.shuffled = false;
                let rank: HashMap<u64, usize> = std::mem::take(&mut self.unshuffled)
                    .into_iter()
                    .enumerate()
                    .map(|(at, key)| (key, at))
                    .collect();
                let rank = |key: u64| rank.get(&key).copied().unwrap_or(usize::MAX);
                // Stable, so anything with no earlier place keeps its order.
                self.upcoming.sort_by_key(|e| rank(e.key));
            }
            QueueAction::Shuffle(_) => {}
            QueueAction::Clear => {
                self.upcoming.clear();
                self.unshuffled.clear();
            }
        }
        self.forget_gone();
    }

    /// Step back from `playing` (B464/V406): the song before it plays
    /// next, without reseeding the album part, and `playing` goes back to
    /// the front of the queue so Next returns to it. `None` when nothing
    /// played before it, or the queue has not heard `playing` start yet.
    pub fn back(&mut self, playing: Track) -> Option<ItemRef> {
        if self.current != Some(playing.id) {
            return None;
        }
        let (front, source) = *self.behind.front()?;
        if front != playing.id {
            return None;
        }
        let previous = *self.behind.get(1)?;
        self.behind.drain(..2);
        let again = self.entry(playing, source);
        if self.shuffled {
            self.unshuffled.insert(0, again.key);
        }
        self.upcoming.insert(0, again);
        self.returning = Some(previous);
        self.from_start = Some(previous.0);
        Some(previous.0)
    }

    fn behind_push(&mut self, id: ItemRef, source: Source) {
        if self.behind.front().map(|(b, _)| *b) != Some(id) {
            self.behind.push_front((id, source));
            self.behind.truncate(RECENT);
        }
    }

    fn position(&self, key: u64) -> Option<usize> {
        self.upcoming.iter().position(|e| e.key == key)
    }

    /// Keep the unshuffled order to the entries still queued.
    fn forget_gone(&mut self) {
        if self.unshuffled.is_empty() {
            return;
        }
        let queued: HashSet<u64> = self.upcoming.iter().map(|e| e.key).collect();
        self.unshuffled.retain(|k| queued.contains(k));
    }
}

impl Queue {
    /// The queue as a reload reads it back with [`Queue::restore`]. The
    /// transient step-back state is left out: a reload ends the step.
    pub fn save(&self) -> String {
        let saved = Saved {
            v: SAVED_VERSION,
            upcoming: self
                .upcoming
                .iter()
                .map(|e| SavedEntry {
                    key: e.key,
                    id: e.id.to_string(),
                    name: e.name.clone(),
                    runtime_ms: e.runtime_ms,
                    source: e.source,
                })
                .collect(),
            shuffled: self.shuffled,
            unshuffled: self.unshuffled.clone(),
            recent: self.recent.iter().map(ToString::to_string).collect(),
            behind: self
                .behind
                .iter()
                .map(|(id, source)| (id.to_string(), *source))
                .collect(),
            current: self.current.map(|id| id.to_string()),
            next_key: self.next_key,
            user: self.owner.clone(),
        };
        // Strings, numbers and bools only: serialising cannot fail.
        serde_json::to_string(&saved).unwrap_or_default()
    }

    /// A queue [`Queue::save`] wrote, marked as restored so the playing
    /// song resuming does not count as a new start.
    pub fn restore(text: &str) -> Result<Queue, RestoreError> {
        let saved: Saved =
            serde_json::from_str(text).map_err(|e| RestoreError::Malformed(e.to_string()))?;
        if saved.v != SAVED_VERSION {
            return Err(RestoreError::Version(saved.v));
        }
        let id = |s: &str| {
            s.parse::<ItemRef>()
                .map_err(|_| RestoreError::BadId(s.to_owned()))
        };
        let upcoming = saved
            .upcoming
            .into_iter()
            .map(|e| {
                Ok(Entry {
                    key: e.key,
                    id: id(&e.id)?,
                    name: e.name,
                    runtime_ms: e.runtime_ms,
                    source: e.source,
                })
            })
            .collect::<Result<Vec<_>, RestoreError>>()?;
        let recent = saved
            .recent
            .iter()
            .map(|s| id(s))
            .collect::<Result<VecDeque<_>, _>>()?;
        let behind = saved
            .behind
            .iter()
            .map(|(s, source)| Ok((id(s)?, *source)))
            .collect::<Result<VecDeque<_>, RestoreError>>()?;
        let current = saved.current.as_deref().map(id).transpose()?;
        // A key at or below one in use would make two rows one.
        let top = upcoming.iter().map(|e| e.key).max().unwrap_or(0);
        let mut queue = Queue {
            upcoming,
            shuffled: saved.shuffled,
            unshuffled: saved.unshuffled,
            recent,
            behind,
            current,
            next_key: saved.next_key.max(top),
            restored: true,
            owner: saved.user,
            ..Queue::default()
        };
        queue.forget_gone();
        if queue.owner.is_none() {
            // Nothing to show is nobody's: an empty record reads as empty.
            return if queue.upcoming.is_empty()
                && queue.behind.is_empty()
                && queue.current.is_none()
            {
                Ok(Queue::default())
            } else {
                Err(RestoreError::Unbound)
            };
        }
        Ok(queue)
    }

    /// Ties the queue to the signed-in `user`. A queue built for another
    /// user is emptied first (returns true): a reload whose stored copy was
    /// not cleared at sign-out never shows one user's songs to the next.
    pub fn claim(&mut self, user: &str) -> bool {
        let other = self.owner.as_deref().is_some_and(|owner| owner != user);
        if other {
            *self = Queue::default();
        }
        self.owner = Some(user.to_owned());
        other
    }

    /// Whom the queue was built for, once a user is known.
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
}

impl Queue {
    /// Where a song that ended goes, by [`Queue::next_after`], and the log
    /// line that says which and why: `from=queue`, `from=album` (the queue
    /// had not heard of the song) or `from=none`.
    pub fn advance(
        &self,
        playing: ItemRef,
        album_next: Option<ItemRef>,
    ) -> (Option<ItemRef>, String) {
        let to = self.next_after(playing, album_next);
        let from = match (self.next(), to) {
            (Some(_), _) => "queue",
            (None, Some(_)) => "album",
            (None, None) => "none",
        };
        let next = to.map_or_else(|| "none".to_owned(), |id| id.to_string());
        (
            to,
            format!("pharos: queue advance reason=ended from={from} next={next}"),
        )
    }
}

/// The log line for songs queued from a page.
pub fn added_line(place: &str, target: &str, count: usize) -> String {
    format!("pharos: queue add place={place} target={target} count={count}")
}

/// Fisher–Yates with a small mixing generator: a play order needs no
/// cryptographic randomness, and a seed keeps the tests exact.
fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut state = seed;
    for i in (1..items.len()).rev() {
        state = splitmix(state);
        // `i + 1` fits in u64 and the result is at most `i`.
        #[allow(clippy::cast_possible_truncation)]
        let j = (state % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

fn splitmix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn track(id: u64) -> Track {
        Track {
            id: ItemRef::Real(id),
            name: format!("Song {id}"),
            runtime_ms: 1_000,
        }
    }

    fn item(id: u64) -> Item {
        Item {
            id: ItemRef::Real(id),
            kind: ItemKind::Audio,
            name: format!("Song {id}"),
            year: None,
            runtime_ms: 1_000,
            image: None,
            played: false,
            favorite: false,
            position_ms: 0,
            series_name: None,
            season: None,
            episode: None,
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
            damaged: false,
            new_children: 0,
        }
    }

    fn ids(q: &Queue) -> Vec<u64> {
        q.upcoming()
            .iter()
            .map(|e| match e.id {
                ItemRef::Real(n) => n,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn album(ids: &[u64]) -> Vec<Item> {
        ids.iter().copied().map(item).collect()
    }

    /// FR-704: a reload reads the queue back as the listener left it, and
    /// the playing song resuming keeps it. Disarm check: without the
    /// `restored` guard in `started`, the album part is reseeded and the
    /// removed song comes back.
    #[test]
    fn the_queue_survives_a_reload_as_left() {
        let mut q = Queue::default();
        q.claim("u1");
        q.started(ItemRef::Real(1), Some(&album(&[2, 3, 4])), 0);
        q.add(vec![track(9)]);
        let three = q.upcoming()[1].key;
        q.apply(QueueAction::Remove(three), 0);
        q.apply(QueueAction::Shuffle(true), 7);
        let shuffled = ids(&q);

        let mut back = Queue::restore(&q.save()).unwrap();
        assert_eq!(ids(&back), shuffled);
        assert!(back.shuffled());
        // The playing song starts again after the reload.
        back.started(ItemRef::Real(1), Some(&album(&[2, 3, 4])), 0);
        assert_eq!(ids(&back), shuffled, "the removed song stays removed");
        // Shuffle off brings back the order from before it.
        back.apply(QueueAction::Shuffle(false), 0);
        assert_eq!(ids(&back), [2, 4, 9]);
        // A later song is a new start, as before the reload.
        back.started(ItemRef::Real(20), Some(&album(&[21])), 0);
        assert_eq!(ids(&back), [9, 21]);
    }

    #[test]
    fn a_restored_queue_steps_back_and_adds_rows_of_its_own() {
        let mut q = Queue::default();
        q.claim("u1");
        q.started(ItemRef::Real(1), Some(&album(&[2])), 0);
        q.started(ItemRef::Real(2), Some(&album(&[])), 0);
        let mut back = Queue::restore(&q.save()).unwrap();
        back.started(ItemRef::Real(2), None, 0);
        // Previous still knows the song before the reload.
        assert_eq!(back.back(track(2)), Some(ItemRef::Real(1)));
        back.add(vec![track(5), track(5)]);
        let keys: HashSet<u64> = back.upcoming().iter().map(|e| e.key).collect();
        assert_eq!(
            keys.len(),
            back.upcoming().len(),
            "every row keeps its own key"
        );
    }

    #[test]
    fn a_saved_queue_another_version_wrote_or_a_broken_one_is_not_read() {
        let saved = Queue::default().save();
        let other = saved.replace("\"v\":1", "\"v\":2");
        assert_eq!(Queue::restore(&other), Err(RestoreError::Version(2)));
        assert!(matches!(
            Queue::restore("{"),
            Err(RestoreError::Malformed(_))
        ));
        let bad = saved.replace("\"current\":null", "\"current\":\"x1\"");
        assert_eq!(Queue::restore(&bad), Err(RestoreError::BadId("x1".into())));
    }

    #[test]
    fn a_saved_queue_is_only_shown_to_the_user_it_was_built_for() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3])), 0);
        // Songs saved before anyone was known are nobody's to show.
        assert_eq!(Queue::restore(&q.save()), Err(RestoreError::Unbound));
        assert_eq!(
            Queue::restore(&Queue::default().save()),
            Ok(Queue::default())
        );

        assert!(!q.claim("u1"), "a queue no one owned is the first user's");
        let mut back = Queue::restore(&q.save()).unwrap();
        assert_eq!(back.owner(), Some("u1"));
        assert!(!back.claim("u1"), "the same user keeps the queue");
        assert_eq!(ids(&back), [2, 3]);

        let mut theirs = Queue::restore(&q.save()).unwrap();
        assert!(theirs.claim("u2"), "another user's queue is dropped");
        assert!(theirs.upcoming().is_empty());
        assert_eq!(theirs.owner(), Some("u2"));
    }

    #[test]
    fn play_next_goes_first_and_add_goes_last() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3])), 0);
        q.add(vec![track(9)]);
        q.play_next(vec![track(8)]);
        assert_eq!(ids(&q), [8, 2, 3, 9]);
        assert_eq!(q.next(), Some(ItemRef::Real(8)));
    }

    #[test]
    fn a_new_album_replaces_the_album_part_and_keeps_what_was_added() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3])), 0);
        q.add(vec![track(9)]);
        q.started(ItemRef::Real(20), Some(&album(&[21, 22])), 0);
        assert_eq!(ids(&q), [9, 21, 22]);
    }

    #[test]
    fn the_queues_first_song_starting_takes_it_out() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3])), 0);
        q.add(vec![track(9)]);
        q.started(ItemRef::Real(2), Some(&album(&[3])), 0);
        assert_eq!(ids(&q), [3, 9]);
        // The same song queued twice plays twice.
        q.play_next(vec![track(2)]);
        q.started(ItemRef::Real(2), Some(&album(&[3])), 0);
        assert_eq!(ids(&q), [3, 9]);
    }

    #[test]
    fn a_song_started_outside_the_queue_keeps_what_was_added_even_if_queued_later() {
        let mut q = Queue::default();
        q.add(vec![track(11), track(12), track(7)]);
        // Song 7 from its album page, not from the queue: the songs added
        // ahead of it stay.
        q.started(ItemRef::Real(7), Some(&album(&[8])), 0);
        assert_eq!(ids(&q), [11, 12, 7, 8]);
    }

    #[test]
    fn the_album_part_holds_only_songs_and_an_unread_album_changes_nothing() {
        let mut q = Queue::default();
        let mut clip = item(5);
        clip.kind = ItemKind::Movie;
        q.started(ItemRef::Real(1), Some(&[item(2), clip]), 0);
        assert_eq!(ids(&q), [2]);
        q.started(ItemRef::Real(40), None, 0);
        assert_eq!(ids(&q), [2]);
    }

    #[test]
    fn once_the_queue_knows_the_song_a_cleared_queue_plays_nothing_more() {
        let mut q = Queue::default();
        let (one, album_next) = (ItemRef::Real(1), Some(ItemRef::Real(2)));
        // Before the queue hears of the song, its album's next stands in.
        assert_eq!(q.next_after(one, album_next), album_next);
        q.started(one, Some(&album(&[2, 3])), 0);
        assert_eq!(q.next_after(one, album_next), album_next);
        q.apply(QueueAction::Clear, 0);
        assert_eq!(q.next_after(one, album_next), None);
    }

    #[test]
    fn picking_an_entry_skips_to_it() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3, 4])), 0);
        let key = q.upcoming()[1].key;
        q.apply(QueueAction::Play(key), 0);
        assert_eq!(q.next(), Some(ItemRef::Real(3)));
        q.started(ItemRef::Real(3), Some(&album(&[4])), 0);
        assert_eq!(ids(&q), [4]);
    }

    #[test]
    fn the_same_song_twice_is_two_entries() {
        let mut q = Queue::default();
        q.add(vec![track(5)]);
        q.add(vec![track(5)]);
        let keys: Vec<u64> = q.upcoming().iter().map(|e| e.key).collect();
        assert_ne!(keys[0], keys[1]);
        q.apply(QueueAction::Remove(keys[1]), 0);
        assert_eq!(ids(&q), [5]);
    }

    #[test]
    fn a_move_puts_the_entry_where_it_was_dropped() {
        let mut q = Queue::default();
        for id in [1, 2, 3, 4] {
            q.add(vec![track(id)]);
        }
        let first = q.upcoming()[0].key;
        q.apply(QueueAction::Move { key: first, to: 2 }, 0);
        assert_eq!(ids(&q), [2, 3, 1, 4]);
        let last = q.upcoming()[3].key;
        q.apply(QueueAction::Move { key: last, to: 0 }, 0);
        assert_eq!(ids(&q), [4, 2, 3, 1]);
        // Past the end lands at the end.
        q.apply(QueueAction::Move { key: last, to: 99 }, 0);
        assert_eq!(ids(&q), [2, 3, 1, 4]);
    }

    #[test]
    fn shuffle_mixes_the_queue_and_turning_it_off_restores_the_order() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(0), Some(&album(&[1, 2, 3, 4, 5, 6, 7, 8])), 0);
        q.apply(QueueAction::Shuffle(true), 42);
        assert!(q.shuffled());
        let mixed = ids(&q);
        assert_ne!(mixed, [1, 2, 3, 4, 5, 6, 7, 8]);
        let mut sorted = mixed.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [1, 2, 3, 4, 5, 6, 7, 8]);

        // Songs come and go while shuffled; the old order still comes back.
        q.apply(QueueAction::Remove(q.upcoming()[0].key), 0);
        q.play_next(vec![track(50)]);
        q.add(vec![track(60)]);
        q.apply(QueueAction::Shuffle(false), 0);
        let mut want: Vec<u64> = vec![50, 1, 2, 3, 4, 5, 6, 7, 8, 60];
        want.retain(|id| *id == 50 || *id == 60 || mixed[1..].contains(id));
        assert_eq!(ids(&q), want);
    }

    #[test]
    fn a_new_album_while_shuffled_comes_in_shuffled() {
        let mut q = Queue::default();
        q.apply(QueueAction::Shuffle(true), 1);
        q.started(ItemRef::Real(0), Some(&album(&[1, 2, 3, 4, 5, 6, 7, 8])), 7);
        assert_ne!(ids(&q), [1, 2, 3, 4, 5, 6, 7, 8]);
        q.apply(QueueAction::Shuffle(false), 0);
        assert_eq!(ids(&q), [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn clear_empties_it() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3])), 0);
        q.apply(QueueAction::Clear, 0);
        assert!(q.upcoming().is_empty());
        assert_eq!(q.next(), None);
    }

    #[test]
    fn a_whole_album_plays_next_in_its_own_order() {
        let mut q = Queue::default();
        q.add(vec![track(9)]);
        q.play_next(vec![track(1), track(2), track(3)]);
        assert_eq!(ids(&q), [1, 2, 3, 9]);
        q.add(vec![track(4), track(5)]);
        assert_eq!(ids(&q), [1, 2, 3, 9, 4, 5]);
    }

    #[test]
    fn a_mix_can_be_cleared_without_touching_the_rest() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2])), 0);
        q.add(vec![track(9)]);
        q.append(vec![track(30), track(31)], Source::Mix);
        assert_eq!(q.upcoming()[3].source, Source::Mix);
        // A new album leaves the mix alone too.
        q.started(ItemRef::Real(20), Some(&album(&[21])), 0);
        assert_eq!(ids(&q), [9, 30, 31, 21]);
        q.drop_source(Source::Mix);
        assert_eq!(ids(&q), [9, 21]);
    }

    #[test]
    fn recent_lists_the_songs_started_newest_first() {
        let mut q = Queue::default();
        for id in 0..(RECENT as u64 + 5) {
            q.started(ItemRef::Real(id), Some(&[]), 0);
        }
        // A remount that starts the same song again counts once.
        q.started(ItemRef::Real(RECENT as u64 + 4), Some(&[]), 0);
        let recent: Vec<ItemRef> = q.recent().collect();
        assert_eq!(recent.len(), RECENT);
        assert_eq!(recent[0], ItemRef::Real(RECENT as u64 + 4));
        assert_eq!(recent[1], ItemRef::Real(RECENT as u64 + 3));
    }

    #[test]
    fn back_then_next_plays_the_same_song_again() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3])), 0);
        q.started(ItemRef::Real(2), Some(&album(&[3])), 0);
        assert_eq!(q.back(track(2)), Some(ItemRef::Real(1)));
        assert!(q.expects(ItemRef::Real(1)));
        assert!(q.stepping_back());
        assert!(q.plays_from_start(ItemRef::Real(1)));
        // The song stepped back to starts without reseeding the album.
        q.started(ItemRef::Real(1), Some(&album(&[2, 3])), 0);
        assert_eq!(ids(&q), [2, 3]);
        assert_eq!(q.next(), Some(ItemRef::Real(2)));
        assert!(!q.stepping_back());
        // Still from the start if the song remounts; not once another starts.
        assert!(q.plays_from_start(ItemRef::Real(1)));
        q.started(ItemRef::Real(2), None, 0);
        assert!(!q.plays_from_start(ItemRef::Real(1)));
    }

    #[test]
    fn a_queued_copy_of_the_song_stepped_back_to_stays_queued() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&[]), 0);
        q.started(ItemRef::Real(2), Some(&[]), 0);
        assert_eq!(q.back(track(2)), Some(ItemRef::Real(1)));
        // While the step back loads, the listener queues song 1 again.
        q.play_next(vec![track(1)]);
        q.started(ItemRef::Real(1), None, 0);
        assert!(!q.stepping_back());
        assert_eq!(ids(&q)[0], 1, "the queued copy is still first");
    }

    #[test]
    fn two_steps_back_walk_what_played() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&[]), 0);
        q.play_next(vec![track(5)]);
        q.started(ItemRef::Real(5), Some(&[]), 0);
        q.append(vec![track(30)], Source::Mix);
        q.started(ItemRef::Real(30), Some(&[]), 0);
        assert_eq!(q.back(track(30)), Some(ItemRef::Real(5)));
        q.started(ItemRef::Real(5), None, 0);
        assert_eq!(q.back(track(5)), Some(ItemRef::Real(1)));
        q.started(ItemRef::Real(1), None, 0);
        assert_eq!(ids(&q), [5, 30]);
        // Each keeps how it got into the queue.
        assert_eq!(q.upcoming()[0].source, Source::Added);
        assert_eq!(q.upcoming()[1].source, Source::Mix);
        assert_eq!(q.back(track(1)), None);
    }

    #[test]
    fn back_needs_a_song_before_and_the_playing_song_heard() {
        let mut q = Queue::default();
        assert_eq!(q.back(track(1)), None);
        q.started(ItemRef::Real(1), Some(&album(&[2])), 0);
        assert_eq!(q.back(track(1)), None);
        // A song the queue has not heard start yet.
        assert_eq!(q.back(track(2)), None);
        assert_eq!(ids(&q), [2]);
    }

    #[test]
    fn back_while_shuffled_keeps_the_song_when_shuffle_goes_off() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), Some(&album(&[2, 3, 4])), 0);
        q.apply(QueueAction::Shuffle(true), 7);
        let first = q.next().unwrap();
        q.started(first, None, 0);
        let ItemRef::Real(n) = first else {
            panic!("{first:?}")
        };
        assert_eq!(q.back(track(n)), Some(ItemRef::Real(1)));
        q.started(ItemRef::Real(1), None, 0);
        q.apply(QueueAction::Shuffle(false), 0);
        assert_eq!(q.next(), Some(first));
        assert_eq!(q.upcoming().len(), 3);
    }

    #[test]
    fn an_ended_song_plays_the_queue_first_then_its_album_and_says_which() {
        let (one, album_next) = (ItemRef::Real(1), Some(ItemRef::Real(2)));
        let mut q = Queue::default();
        let (to, line) = q.advance(one, album_next);
        assert_eq!(to, album_next);
        assert!(line.contains("from=album"), "{line}");
        q.started(one, Some(&album(&[2])), 0);
        q.play_next(vec![track(8)]);
        let (to, line) = q.advance(one, album_next);
        assert_eq!(to, Some(ItemRef::Real(8)));
        assert!(line.contains("from=queue"), "{line}");
        assert!(
            line.contains(&format!("next={}", ItemRef::Real(8))),
            "{line}"
        );
        q.apply(QueueAction::Clear, 0);
        let (to, line) = q.advance(one, album_next);
        assert_eq!(to, None);
        assert!(line.contains("from=none next=none"), "{line}");
    }

    #[test]
    fn an_add_logs_where_what_and_how_many() {
        assert_eq!(
            added_line("next", "album_card", 12),
            "pharos: queue add place=next target=album_card count=12"
        );
    }

    #[test]
    fn the_action_labels_are_distinct() {
        let labels = [
            QueueAction::Play(0).label(),
            QueueAction::Remove(0).label(),
            QueueAction::Move { key: 0, to: 0 }.label(),
            QueueAction::Shuffle(true).label(),
            QueueAction::Shuffle(false).label(),
            QueueAction::Clear.label(),
        ];
        for (i, a) in labels.iter().enumerate() {
            assert!(!labels[i + 1..].contains(a), "{a}");
        }
    }

    #[test]
    fn the_source_labels_are_distinct() {
        let labels = [
            Source::Added.label(),
            Source::Album.label(),
            Source::Mix.label(),
        ];
        for (i, a) in labels.iter().enumerate() {
            assert!(!labels[i + 1..].contains(a), "{a}");
        }
    }

    /// V853: a video's queue row names an episode with its show and number;
    /// a film keeps its own name.
    #[test]
    fn a_queued_video_is_named_with_its_show() {
        let episode = Item {
            kind: ItemKind::Episode,
            name: "The Paper Boat".to_owned(),
            series_name: Some("Show X".to_owned()),
            season: Some(3),
            episode: Some(10),
            ..item(4)
        };
        assert_eq!(
            Track::video(&episode).name,
            "Show X · S3 · E10 · The Paper Boat"
        );
        let film = Item {
            kind: ItemKind::Movie,
            name: "Film BB".to_owned(),
            ..item(5)
        };
        assert_eq!(Track::video(&film).name, "Film BB");
        assert!(video_kind(ItemKind::Movie) && video_kind(ItemKind::Episode));
        assert!(!video_kind(ItemKind::Audio) && !video_kind(ItemKind::Series));
    }

    /// V853: a queued video that starts leaves the queue, from wherever it
    /// is; one not queued leaves the queue as it was. The video playing is
    /// never its own next.
    #[test]
    fn a_queued_video_leaves_the_queue_when_it_starts() {
        let mut q = Queue::default();
        q.add(vec![
            Track::from(&item(1)),
            Track::from(&item(2)),
            Track::from(&item(3)),
        ]);
        assert!(!q.take(ItemRef::Real(9)), "a video not queued");
        assert_eq!(q.upcoming().len(), 3);
        assert!(q.take(ItemRef::Real(2)));
        assert_eq!(q.next(), Some(ItemRef::Real(1)));
        assert_eq!(q.next_besides(ItemRef::Real(1)), Some(ItemRef::Real(3)));
        assert_eq!(q.next_besides(ItemRef::Real(3)), Some(ItemRef::Real(1)));
    }

    /// V853: the queued video comes before the next episode, a group's
    /// next ignores the queue, and the log line names inputs and verdict.
    #[test]
    fn a_videos_next_is_the_queue_then_the_episode_unless_grouped() {
        let (q, e) = (Some(ItemRef::Real(8)), Some(ItemRef::Real(2)));
        assert_eq!(video_next(q, e, false), (q, NextFrom::Queue));
        assert_eq!(video_next(None, e, false), (e, NextFrom::Episode));
        assert_eq!(video_next(q, e, true), (e, NextFrom::Group));
        assert_eq!(video_next(None, None, false), (None, NextFrom::Nothing));
        assert_eq!(
            video_next_line(ItemRef::Real(1), q, e, false),
            "pharos: video next item=1 next=8 from=queue queued=8 episode=2 grouped=false"
        );
        assert_eq!(
            video_next_line(ItemRef::Real(1), None, None, true),
            "pharos: video next item=1 next=none from=group queued=none episode=none grouped=true"
        );
    }
}
