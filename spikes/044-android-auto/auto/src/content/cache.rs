//! The on-disk audio cache: whole tracks kept on the phone so playback
//! survives a black spot.
//!
//! ```text
//! <dir>/index.json                   size, last_played, pinned, complete per track
//! <dir>/<id>.audio                   a finished track (id percent-escaped)
//! <dir>/<id>.audio.part              a download in progress, renamed when whole
//! <dir>/listings/<container>.json    last browse listing, for offline browse
//! ```
//!
//! Every decision is logged with the value that drove it (`cache lookup`,
//! `cache evict`, ...). Tokens never reach a log line or a file here: listings
//! drop stream and art URLs because those carry `api_key`.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::error::ContentError;
use super::node::Node;

/// How much music the phone keeps unless the caller says otherwise.
pub const DEFAULT_CAP_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const INDEX_VERSION: u32 = 1;
const AUDIO_EXT: &str = ".audio";
const PART_EXT: &str = ".audio.part";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub last_played_ms: u64,
    pub pinned: bool,
    pub complete: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: BTreeMap<String, Entry>,
    /// Container id -> the track ids it pinned.
    pins: BTreeMap<String, Vec<String>>,
}

/// What a lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Hit(PathBuf),
    /// Some bytes are on disk but the track is not whole.
    Partial,
    Miss,
}

/// A browse row as kept for offline use: no URLs, so no token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedNode {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub browsable: bool,
    pub playable: bool,
    pub duration_ms: u64,
}

impl SavedNode {
    pub fn from_node(n: &Node) -> Self {
        Self {
            id: n.id.clone(),
            title: n.title.clone(),
            subtitle: n.subtitle.clone(),
            browsable: n.browsable,
            playable: n.playable,
            duration_ms: n.duration_ms,
        }
    }

    /// Without art or stream URL: play it through [`Cache::local_path`].
    pub fn to_node(&self) -> Node {
        Node {
            id: self.id.clone(),
            title: self.title.clone(),
            subtitle: self.subtitle.clone(),
            browsable: self.browsable,
            playable: self.playable,
            art_url: None,
            stream_url: None,
            duration_ms: self.duration_ms,
        }
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    pins: BTreeMap<String, Vec<String>>,
    /// The latest prefetch request: never evicted while it is current.
    protected: HashSet<String>,
    /// The track last handed to the player.
    current: Option<String>,
    clock: u64,
}

pub struct Cache {
    dir: PathBuf,
    cap: u64,
    state: Mutex<State>,
}

fn err(op: &str, path: &Path, e: impl std::fmt::Display) -> ContentError {
    ContentError::Cache {
        op: op.to_owned(),
        why: format!("{}: {e}", path.display()),
    }
}

/// Escape everything but `[A-Za-z0-9_-]` as `%XX`, so ids map to distinct
/// plain file names and `..` or `/` can never leave the directory.
fn escape(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unescape(name: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(name.len());
    let mut it = name.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hi = it.next()?;
            let lo = it.next()?;
            let hex = [hi, lo];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Write `bytes` to `path` whole: temp file, fsync, rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

impl Cache {
    /// Open (creating) the cache in `dir`. Leftover `.part` files are removed,
    /// files the index disagrees with are dropped or adopted, and the cache is
    /// trimmed to `cap_bytes`.
    pub fn open(dir: &Path, cap_bytes: u64) -> Result<Self, ContentError> {
        std::fs::create_dir_all(dir.join("listings")).map_err(|e| err("create dir", dir, e))?;
        let index_path = dir.join("index.json");
        let mut index = match std::fs::read(&index_path) {
            Ok(b) => match serde_json::from_slice::<IndexFile>(&b) {
                Ok(i) if i.version == INDEX_VERSION => i,
                Ok(i) => {
                    log::warn!(
                        "cache open: index version {} is not {INDEX_VERSION}; rebuilding from files",
                        i.version
                    );
                    IndexFile::default()
                }
                Err(e) => {
                    log::warn!("cache open: index unreadable ({e}); rebuilding from files");
                    IndexFile::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => IndexFile::default(),
            Err(e) => return Err(err("read index", &index_path, e)),
        };
        index.version = INDEX_VERSION;

        let (mut parts, mut dropped, mut adopted) = (0u32, 0u32, 0u32);
        let mut on_disk: BTreeMap<String, u64> = BTreeMap::new();
        for ent in std::fs::read_dir(dir).map_err(|e| err("list dir", dir, e))? {
            let Ok(ent) = ent else { continue };
            let path = ent.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if name.ends_with(PART_EXT) || name.ends_with(".tmp") {
                match std::fs::remove_file(&path) {
                    Ok(()) => parts = parts.saturating_add(1),
                    Err(e) => log::warn!("cache open: cannot remove {name}: {e}"),
                }
            } else if let Some(stem) = name.strip_suffix(AUDIO_EXT) {
                if let (Some(id), Ok(meta)) = (unescape(stem), ent.metadata()) {
                    on_disk.insert(id, meta.len());
                }
            }
        }
        for (id, size) in &on_disk {
            match index.entries.get(id) {
                Some(e) if e.complete && e.size == *size => {}
                Some(e) => {
                    log::warn!(
                        "cache open: id={id} index says size={} complete={} but file is {size} bytes; removing",
                        e.size, e.complete
                    );
                    let _ = std::fs::remove_file(dir.join(format!("{}{AUDIO_EXT}", escape(id))));
                    index.entries.remove(id);
                    dropped = dropped.saturating_add(1);
                }
                None => {
                    index.entries.insert(
                        id.clone(),
                        Entry {
                            size: *size,
                            last_played_ms: 0,
                            pinned: false,
                            complete: true,
                        },
                    );
                    adopted = adopted.saturating_add(1);
                }
            }
        }
        index.entries.retain(|id, e| {
            let keep = if e.complete {
                on_disk.get(id) == Some(&e.size)
            } else {
                e.pinned
            };
            if !keep {
                log::warn!("cache open: id={id} is in the index but not on disk; forgetting");
                dropped = dropped.saturating_add(1);
            }
            keep
        });

        let clock = index
            .entries
            .values()
            .map(|e| e.last_played_ms)
            .max()
            .unwrap_or(0);
        let cache = Self {
            dir: dir.to_owned(),
            cap: cap_bytes,
            state: Mutex::new(State {
                entries: index.entries,
                pins: index.pins,
                clock,
                ..State::default()
            }),
        };
        {
            let mut s = cache.lock();
            cache.evict_locked(&mut s);
            let (n, bytes) = Self::totals(&s);
            log::info!(
                "cache open dir={} tracks={n} bytes={bytes} cap={cap_bytes} stray_part_removed={parts} dropped={dropped} adopted={adopted}",
                dir.display()
            );
            cache.persist_locked(&s);
        }
        Ok(cache)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn totals(s: &State) -> (usize, u64) {
        s.entries
            .values()
            .filter(|e| e.complete)
            .fold((0, 0u64), |(n, b), e| {
                (n.saturating_add(1), b.saturating_add(e.size))
            })
    }

    /// Bytes held by finished tracks (an in-progress `.part` is not counted).
    pub fn used_bytes(&self) -> u64 {
        Self::totals(&self.lock()).1
    }

    pub fn final_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}{AUDIO_EXT}", escape(id)))
    }

    pub fn part_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}{PART_EXT}", escape(id)))
    }

    pub fn entry(&self, id: &str) -> Option<Entry> {
        self.lock().entries.get(id).cloned()
    }

    pub fn is_complete(&self, id: &str) -> bool {
        self.lock().entries.get(id).is_some_and(|e| e.complete)
    }

    /// What the cache holds for `id`; logged, does not count as a play.
    pub fn lookup(&self, id: &str) -> Lookup {
        let complete = self.is_complete(id);
        let verdict = if complete {
            Lookup::Hit(self.final_path(id))
        } else if std::fs::metadata(self.part_path(id)).is_ok_and(|m| m.len() > 0) {
            Lookup::Partial
        } else {
            Lookup::Miss
        };
        let word = match verdict {
            Lookup::Hit(_) => "hit",
            Lookup::Partial => "partial",
            Lookup::Miss => "miss",
        };
        log::info!("cache lookup id={id} verdict={word}");
        verdict
    }

    /// The finished file for `id`, counted as a play (it becomes the newest
    /// for eviction and is never evicted while current).
    pub fn local_path(&self, id: &str) -> Option<PathBuf> {
        let mut s = self.lock();
        let hit = s.entries.get(id).is_some_and(|e| e.complete);
        let path = self.final_path(id);
        let present = hit && path.is_file();
        log::info!(
            "cache lookup id={id} verdict={}",
            if present { "hit" } else { "miss" }
        );
        if !present {
            if hit {
                log::warn!("cache id={id} is indexed but its file is gone; forgetting");
                s.entries.remove(id);
                self.persist_locked(&s);
            }
            return None;
        }
        let t = Self::tick(&mut s);
        if let Some(e) = s.entries.get_mut(id) {
            e.last_played_ms = t;
        }
        s.current = Some(id.to_owned());
        self.persist_locked(&s);
        Some(path)
    }

    /// Record a finished download: `.part` becomes `.audio`, the entry
    /// complete, then the cache is trimmed. Returns the evicted ids.
    pub fn commit(&self, id: &str, expected: Option<u64>) -> Result<Vec<String>, ContentError> {
        let part = self.part_path(id);
        let size = std::fs::metadata(&part)
            .map_err(|e| err("stat part", &part, e))?
            .len();
        if let Some(want) = expected {
            if want != size {
                return Err(ContentError::Cache {
                    op: "commit".into(),
                    why: format!("id={id} part is {size} bytes, server said {want}"),
                });
            }
        }
        std::fs::File::open(&part)
            .and_then(|f| f.sync_all())
            .map_err(|e| err("sync part", &part, e))?;
        let fin = self.final_path(id);
        std::fs::rename(&part, &fin).map_err(|e| err("rename part", &fin, e))?;
        let mut s = self.lock();
        let t = Self::tick(&mut s);
        let pinned = s.pins.values().any(|v| v.iter().any(|p| p == id));
        s.entries.insert(
            id.to_owned(),
            Entry {
                size,
                last_played_ms: t,
                pinned,
                complete: true,
            },
        );
        let evicted = self.evict_locked(&mut s);
        self.persist_locked(&s);
        Ok(evicted)
    }

    /// Remove the in-progress file for `id`.
    pub fn discard_part(&self, id: &str) {
        let p = self.part_path(id);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("cache id={id}: cannot remove part: {e}"),
        }
    }

    /// The ids the player is about to need; they are exempt from eviction
    /// until the next call replaces them.
    pub fn protect(&self, ids: &[String]) {
        self.lock().protected = ids.iter().cloned().collect();
    }

    /// Pin every id of `container`: kept regardless of the cap. Returns how
    /// many of them are already on disk.
    pub fn pin(&self, container: &str, ids: &[String]) -> usize {
        let mut s = self.lock();
        s.pins.insert(container.to_owned(), ids.to_vec());
        for id in ids {
            s.entries
                .entry(id.clone())
                .and_modify(|e| e.pinned = true)
                .or_insert(Entry {
                    size: 0,
                    last_played_ms: 0,
                    pinned: true,
                    complete: false,
                });
        }
        let have = ids
            .iter()
            .filter(|id| s.entries.get(*id).is_some_and(|e| e.complete))
            .count();
        log::info!(
            "cache pin container={container} tracks={} cached={have}",
            ids.len()
        );
        self.persist_locked(&s);
        have
    }

    /// Release `container`'s pins; its tracks become ordinary LRU entries.
    pub fn unpin(&self, container: &str) {
        let mut s = self.lock();
        let Some(ids) = s.pins.remove(container) else {
            log::info!("cache unpin container={container} was_pinned=false");
            return;
        };
        let still: HashSet<String> = s.pins.values().flatten().cloned().collect();
        for id in &ids {
            if still.contains(id) {
                continue;
            }
            let drop_placeholder = match s.entries.get_mut(id) {
                Some(e) if e.complete => {
                    e.pinned = false;
                    false
                }
                Some(_) => true,
                None => false,
            };
            if drop_placeholder {
                s.entries.remove(id);
            }
        }
        log::info!(
            "cache unpin container={container} was_pinned=true tracks={}",
            ids.len()
        );
        self.evict_locked(&mut s);
        self.persist_locked(&s);
    }

    /// Track ids pinned through `container`, if it is pinned.
    pub fn pinned_tracks(&self, container: &str) -> Option<Vec<String>> {
        self.lock().pins.get(container).cloned()
    }

    pub fn is_pinned(&self, container: &str) -> bool {
        self.lock().pins.contains_key(container)
    }

    /// Strictly increasing so two plays in one millisecond still order.
    fn tick(s: &mut State) -> u64 {
        s.clock = now_ms().max(s.clock.saturating_add(1));
        s.clock
    }

    /// Delete least-recently-played unpinned finished tracks until the cache
    /// fits; skip the current and protected ones.
    fn evict_locked(&self, s: &mut State) -> Vec<String> {
        let mut evicted = Vec::new();
        loop {
            let (_, used) = Self::totals(s);
            if used <= self.cap {
                break;
            }
            let victim = s
                .entries
                .iter()
                .filter(|(id, e)| {
                    e.complete
                        && !e.pinned
                        && !s.protected.contains(*id)
                        && s.current.as_deref() != Some(id.as_str())
                })
                .min_by(|a, b| (a.1.last_played_ms, a.0).cmp(&(b.1.last_played_ms, b.0)))
                .map(|(id, e)| (id.clone(), e.size));
            let Some((id, bytes)) = victim else {
                log::warn!(
                    "cache over cap used={used} cap={} and nothing is evictable (pinned, queued or playing)",
                    self.cap
                );
                break;
            };
            match std::fs::remove_file(self.final_path(&id)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    log::warn!("cache evict id={id} failed: {e}; keeping it indexed");
                    break;
                }
            }
            s.entries.remove(&id);
            log::info!("cache evict id={id} bytes={bytes} reason=over_cap");
            evicted.push(id);
        }
        evicted
    }

    fn persist_locked(&self, s: &State) {
        let index = IndexFile {
            version: INDEX_VERSION,
            entries: s.entries.clone(),
            pins: s.pins.clone(),
        };
        let path = self.dir.join("index.json");
        let res = serde_json::to_vec(&index)
            .map_err(std::io::Error::other)
            .and_then(|b| write_atomic(&path, &b));
        if let Err(e) = res {
            log::warn!("cache index not saved ({}): {e}", path.display());
        }
    }

    fn listing_path(&self, container: &str) -> PathBuf {
        self.dir
            .join("listings")
            .join(format!("{}.json", escape(container)))
    }

    /// Keep `nodes` as the last known listing of `container`.
    pub fn save_listing(&self, container: &str, nodes: &[Node]) {
        let saved: Vec<SavedNode> = nodes.iter().map(SavedNode::from_node).collect();
        let path = self.listing_path(container);
        let res = serde_json::to_vec(&saved)
            .map_err(std::io::Error::other)
            .and_then(|b| write_atomic(&path, &b));
        if let Err(e) = res {
            log::warn!("cache listing {container} not saved: {e}");
        }
    }

    pub fn load_listing(&self, container: &str) -> Option<Vec<SavedNode>> {
        let bytes = std::fs::read(self.listing_path(container)).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!("cache listing {container} unreadable: {e}");
                None
            }
        }
    }

    /// Every saved listing, `(container id, rows)`.
    pub fn all_listings(&self) -> Vec<(String, Vec<SavedNode>)> {
        let Ok(rd) = std::fs::read_dir(self.dir.join("listings")) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let id = unescape(name.strip_suffix(".json")?)?;
                let rows = self.load_listing(&id)?;
                Some((id, rows))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    pub(crate) fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pharos-auto-cache-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A finished track of `n` bytes, as the downloader would leave it.
    pub(crate) fn put(c: &Cache, id: &str, n: usize) {
        std::fs::write(c.part_path(id), vec![7u8; n]).unwrap();
        c.commit(id, Some(n as u64)).unwrap();
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn file_names_are_distinct_and_cannot_leave_the_directory() {
        for id in ["track/1", "track_1", "../../etc/passwd", "a b", "é"] {
            let e = escape(id);
            assert!(!e.contains('/') && !e.contains('.'), "{e}");
            assert_eq!(unescape(&e).as_deref(), Some(id));
        }
        assert_ne!(escape("track/1"), escape("track_1"));
    }

    #[test]
    fn a_committed_track_is_a_hit_and_survives_a_reopen() {
        let d = tmp("reopen");
        let c = Cache::open(&d, 1000).unwrap();
        assert_eq!(c.lookup("track/1"), Lookup::Miss);
        put(&c, "track/1", 100);
        c.pin("album/9", &ids(&["track/1"]));
        let first = c.local_path("track/1").unwrap();
        assert_eq!(std::fs::metadata(&first).unwrap().len(), 100);
        let before = c.entry("track/1").unwrap();
        drop(c);

        let c = Cache::open(&d, 1000).unwrap();
        assert_eq!(c.entry("track/1").unwrap(), before);
        assert_eq!(c.lookup("track/1"), Lookup::Hit(first));
        assert!(c.is_pinned("album/9"));
        assert!(c.entry("track/1").unwrap().pinned);
    }

    #[test]
    fn stray_part_files_are_removed_on_open_and_partial_is_reported_before() {
        let d = tmp("part");
        let c = Cache::open(&d, 1000).unwrap();
        std::fs::write(c.part_path("track/2"), b"half").unwrap();
        assert_eq!(c.lookup("track/2"), Lookup::Partial);
        assert!(c.local_path("track/2").is_none());
        drop(c);
        let c = Cache::open(&d, 1000).unwrap();
        assert!(!c.part_path("track/2").exists());
        assert_eq!(c.lookup("track/2"), Lookup::Miss);
    }

    #[test]
    fn a_missing_or_corrupt_index_is_rebuilt_from_the_files() {
        let d = tmp("rebuild");
        let c = Cache::open(&d, 1000).unwrap();
        put(&c, "track/3", 10);
        drop(c);
        std::fs::write(d.join("index.json"), b"{ not json").unwrap();
        let c = Cache::open(&d, 1000).unwrap();
        assert!(c.is_complete("track/3"));
        assert_eq!(c.entry("track/3").unwrap().size, 10);
    }

    #[test]
    fn a_file_that_disagrees_with_the_index_is_dropped() {
        let d = tmp("mismatch");
        let c = Cache::open(&d, 1000).unwrap();
        put(&c, "track/4", 10);
        let f = c.final_path("track/4");
        drop(c);
        std::fs::write(&f, b"short").unwrap();
        let c = Cache::open(&d, 1000).unwrap();
        assert!(!c.is_complete("track/4"));
        assert!(!f.exists());
    }

    #[test]
    fn eviction_removes_the_least_recently_played_first() {
        let d = tmp("lru");
        let c = Cache::open(&d, 300).unwrap();
        put(&c, "track/a", 100);
        put(&c, "track/b", 100);
        put(&c, "track/c", 100);
        // Playing `a` makes `b` the oldest.
        c.local_path("track/a").unwrap();
        c.protect(&[]);
        put(&c, "track/d", 100);
        assert!(!c.is_complete("track/b"), "b was the least recently played");
        assert!(c.is_complete("track/a") && c.is_complete("track/c") && c.is_complete("track/d"));
        assert!(!c.final_path("track/b").exists());
        assert_eq!(c.used_bytes(), 300);
    }

    #[test]
    fn pinned_tracks_are_never_evicted() {
        let d = tmp("pinned");
        let c = Cache::open(&d, 200).unwrap();
        put(&c, "track/a", 100);
        c.pin("album/1", &ids(&["track/a"]));
        put(&c, "track/b", 100);
        put(&c, "track/c", 100);
        assert!(c.is_complete("track/a"), "pinned stays");
        assert!(!c.is_complete("track/b"));
        assert!(c.is_complete("track/c"));
        // Pinning more than the cap holds leaves it over cap rather than deleting.
        c.pin("album/2", &ids(&["track/c", "track/e"]));
        put(&c, "track/e", 100);
        put(&c, "track/f", 100);
        assert!(c.is_complete("track/a") && c.is_complete("track/c") && c.is_complete("track/e"));
        // Unpinning makes the oldest evictable again.
        c.unpin("album/1");
        assert!(!c.is_complete("track/a"));
        assert!(
            c.entry("track/c").unwrap().pinned,
            "still pinned by album/2"
        );
    }

    #[test]
    fn the_current_and_queued_tracks_are_never_evicted() {
        let d = tmp("protected");
        let c = Cache::open(&d, 200).unwrap();
        put(&c, "track/a", 100);
        put(&c, "track/b", 100);
        c.local_path("track/a").unwrap(); // current
        c.protect(&ids(&["track/b", "track/c"])); // queued next
        put(&c, "track/c", 100);
        assert!(c.is_complete("track/a") && c.is_complete("track/b"));
        assert!(c.is_complete("track/c"), "queued, so kept over cap");
        assert_eq!(c.used_bytes(), 300);
        c.protect(&[]);
        put(&c, "track/d", 100);
        assert!(!c.is_complete("track/b") && !c.is_complete("track/c"));
        assert!(c.is_complete("track/a"), "still current");
    }

    #[test]
    fn a_shrunk_cap_trims_on_open() {
        let d = tmp("shrink");
        let c = Cache::open(&d, 1000).unwrap();
        put(&c, "track/a", 100);
        put(&c, "track/b", 100);
        drop(c);
        let c = Cache::open(&d, 100).unwrap();
        assert_eq!(c.used_bytes(), 100);
        assert!(c.is_complete("track/b"), "the newer one stays");
    }

    #[test]
    fn a_commit_that_disagrees_with_the_server_length_is_refused() {
        let d = tmp("len");
        let c = Cache::open(&d, 1000).unwrap();
        std::fs::write(c.part_path("track/a"), b"1234").unwrap();
        let e = c.commit("track/a", Some(10)).unwrap_err();
        assert!(e.to_string().contains("server said 10"), "{e}");
        assert!(!c.is_complete("track/a"));
    }

    #[test]
    fn listings_round_trip_without_urls() {
        let d = tmp("listing");
        let c = Cache::open(&d, 1000).unwrap();
        let n = Node {
            id: "track/1".into(),
            title: "T".into(),
            subtitle: "A".into(),
            browsable: false,
            playable: true,
            art_url: Some("https://h/x?api_key=SECRET".into()),
            stream_url: Some("https://h/a?api_key=SECRET".into()),
            duration_ms: 5,
        };
        c.save_listing("album/9", &[n]);
        let raw = std::fs::read_to_string(c.listing_path("album/9")).unwrap();
        assert!(!raw.contains("SECRET") && !raw.contains("http"), "{raw}");
        let back = c.load_listing("album/9").unwrap();
        assert_eq!(back[0].to_node().stream_url, None);
        assert_eq!(back[0].id, "track/1");
        assert_eq!(c.all_listings().len(), 1);
        assert!(c.load_listing("album/none").is_none());
    }
}
