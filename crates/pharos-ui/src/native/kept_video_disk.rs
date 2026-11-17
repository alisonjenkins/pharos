//! Downloaded video on a device's disk (spec 048, ADR 0145): the phone's
//! [`VideoStore`], and any other host that has a directory to keep files in.
//!
//! Guarded by V1463 (whole files, path+query keys), V1464 (shared cap and
//! free-space guard) and V1465 (every outcome logged) in
//! `specs/001-pharos-baseline/invariants.md`.
//!
//! The same plan as the web store (`kept_video_web.rs`): the player's own
//! HLS stream, every playlist, init and segment stored under the request
//! key the player will ask for (`request_key`: path and query). Here a key
//! is a file named by its hash under `<root>/<user>/<item>/files/`, beside
//! the item's `manifest.json` (the [`KeptVideo`] record). A file is written
//! to `<name>.part` and renamed once whole, so a file that exists is a file
//! that is complete, and a restart refetches only what is missing.
//!
//! Bytes go from the network to the file chunk by chunk; nothing holds a
//! whole video in memory. What the host brings is behind two small traits,
//! so tests drive the loop with a fake server: [`Fetcher`] (the HTTP the
//! app already uses, with its token and server address) and [`Budget`]
//! (how much may be kept and how much room is left).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use futures_util::stream::{self, StreamExt as _};
use pharos_hls::offline::{estimate_bytes, plan_master, plan_media};
use tokio::io::AsyncWriteExt as _;

use super::kept_video::{
    check_not_kept, content_type_for, find, fits, insert_front, interrupted, is_retryable,
    missing_files, new_record, progress_due, request_key, retry_delay_secs, take, url_path,
    KeepVideoRefusal, KeepVideoRequest, KeptState, KeptVideo, StoredFile, VideoStore,
};
use super::platform;

/// Two files in flight: the link is the limit, and more would only make
/// each segment slower to arrive whole.
const CONCURRENCY: usize = 2;

/// Playlists are planned against this origin, then cut back to keys: the
/// shared planner resolves relative URIs against an absolute URL.
const PLAN_ORIGIN: &str = "http://pharos.invalid";

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a fetch failed: `status` is the server's answer, `None` when there
/// was none (no network, a dropped connection).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.shown())]
pub struct FetchError {
    pub status: Option<u16>,
    pub message: String,
}

impl FetchError {
    fn shown(&self) -> String {
        match self.status {
            Some(s) => format!("status {s}"),
            None => format!("network ({})", self.message),
        }
    }
}

/// A response body, read a chunk at a time.
pub trait Body: Send {
    /// The next chunk, or `None` at the end.
    fn next_chunk(&mut self) -> BoxFut<'_, Result<Option<Vec<u8>>, FetchError>>;
}

/// The app's HTTP to its server: GET a request key (path and query) and
/// stream the body. A non-success status is an `Err` carrying it.
pub trait Fetcher: Send + Sync {
    fn open<'a>(&'a self, key: &'a str) -> BoxFut<'a, Result<Box<dyn Body>, FetchError>>;
}

/// How much video may be kept and how much room there is.
pub trait Budget: Send + Sync {
    /// The most that downloaded media of every kind may take together.
    fn cap_bytes(&self) -> u64;
    /// What the other kinds (the phone's music) hold of that now.
    fn other_used_bytes(&self) -> u64;
    /// Free space on the device, measured now.
    fn free_bytes(&self) -> u64;
    /// Whether the other kinds' figures can be had yet. A host whose music
    /// cache is not up reports the default cap and no music, so a new
    /// download is refused until it is. Ready by default.
    fn accounting_ready(&self) -> bool {
        true
    }
    /// What the other kinds have been approved to write and not yet landed:
    /// the free space does not lack these yet, but will. Read under the
    /// admission lock, before the free space is measured. None by default.
    fn other_outstanding_bytes(&self) -> u64 {
        0
    }
    /// Holds the lock a host shares between its kinds of download while an
    /// admission reads the other kinds' bytes and reserves its own, so
    /// neither kind admits against a figure the other is changing. Taken
    /// before the store's own lock, always. No lock by default.
    fn admission_guard(&self) -> Box<dyn std::any::Any> {
        Box::new(())
    }
}

/// What a host supplies besides the directory.
pub struct Env {
    pub fetcher: Arc<dyn Fetcher>,
    pub budget: Arc<dyn Budget>,
    /// Waits this many seconds (the retry backoff).
    pub sleep: Arc<dyn Fn(u64) -> BoxFut<'static, ()> + Send + Sync>,
    /// The time as ISO 8601 UTC.
    pub now: fn() -> String,
    /// The runtime the downloads run on.
    pub runtime: tokio::runtime::Handle,
    /// Deletes a folder and what is in it (`std::fs::remove_dir_all`; a
    /// test makes it fail).
    pub remove_dir: fn(&Path) -> std::io::Result<()>,
}

/// Why a download stopped before it was ready.
#[derive(Debug)]
enum Stop {
    /// Removed while running: nothing more to record.
    Gone,
    /// Over the cap or short of space; stored playlists are deleted.
    Refused(String),
    /// A file could not be fetched or stored; stored files stay for a retry.
    Failed(String),
}

#[derive(Default)]
struct State {
    list: Vec<KeptVideo>,
    /// Request key to its whole file on disk.
    index: HashMap<(String, String), (PathBuf, u64)>,
    /// The run that owns each (user, item) download; a removed or restarted
    /// download is told apart from the one still running by this number.
    runs: HashMap<(String, String), u64>,
    next_run: u64,
    /// Bytes each run of a download has written to staging files not yet
    /// whole: on disk already, so the free space lacks them, but not yet in
    /// the record's stored bytes. Keyed by run: an old run still draining
    /// after a restart stages bytes the new run's estimate never covered.
    staged: HashMap<(String, String, u64), u64>,
    /// Folders of removed downloads that could not be deleted, with the
    /// bytes left in them: counted against the cap until a later sweep
    /// deletes them.
    tombstones: Vec<(PathBuf, u64)>,
    /// Folders found at open whose record could not be read: kept, never
    /// swept, and counted against the cap at the bytes measured.
    held: Vec<(PathBuf, u64)>,
    /// Folders found at open whose size or record is not known (unlistable,
    /// unreadable, damaged): no download is admitted while one exists. The
    /// reason travels with it. Folders of removed downloads are not here:
    /// whether those are known is derived (`State::unknown_reason`) from
    /// the facts below, never stored as a mark that has to be cleared.
    unmeasured: Vec<(PathBuf, String)>,
    /// Folders a `remove` is working on right now. A sweep leaves them
    /// alone, and while one is listed its folder is of unknown size.
    removing: Vec<PathBuf>,
    /// Staging files being written, by run (a run writes several at once).
    /// A removal revokes its run, but a write already under way goes on to
    /// the next chunk's check: while a revoked run has a writer its folder
    /// is of unknown size.
    writers: HashMap<(String, String, u64), usize>,
    /// Folders a sweep has picked and is deleting. Picked under the same
    /// hold that checked nothing owns them, and refused to `start` until
    /// the delete is over: a download restarted in that gap would have its
    /// new files deleted.
    deleting: Vec<PathBuf>,
    /// Folders where a staging file could not be deleted: nothing bounds
    /// them until a later sweep deletes the file or measures the folder
    /// whole, so they are unknown meanwhile. A fact, so accounting does not
    /// depend on a cleanup having worked.
    leftover: Vec<PathBuf>,
    /// Folders whose leftover a sweep is settling right now. One settlement
    /// per folder at a time, and `start` refuses a folder in it: a second
    /// sweep (or a retry) would otherwise act on what the first one saw
    /// before the folder changed.
    settling: Vec<PathBuf>,
    /// The last measurement of each tombstoned folder: the epoch it was
    /// taken at, and whether the walk read everything.
    measured: HashMap<PathBuf, (u64, bool)>,
    /// Per folder, moved by every event that can change its size under a
    /// measurement: a writer starting or stopping, a removal beginning. A
    /// measurement is valid only at the epoch it was taken at.
    epochs: HashMap<PathBuf, u64>,
    /// The store's root, to tell a record's folder from a tombstone's.
    root: PathBuf,
    last_saved: Option<Instant>,
}

struct Inner {
    root: PathBuf,
    env: Env,
    state: Mutex<State>,
    /// Held while a record is written or an item's folder is removed.
    saves: Mutex<()>,
    #[cfg(test)]
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// The store of downloaded video on disk.
pub struct DiskVideoStore {
    inner: Arc<Inner>,
}

fn emit(warn: bool, line: String) {
    #[cfg(test)]
    tests::CAPTURED.with(|c| c.borrow_mut().push(line.clone()));
    if warn {
        platform::warn(&line);
    } else {
        platform::info(&line);
    }
}

/// A path component made of `name`: letters, digits, `-` and `_` as they
/// are, anything else as `%` and its hex, so no id can climb out of `root`.
fn safe_name(name: &str) -> String {
    if name.is_empty() {
        return "_".to_owned();
    }
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => char::from(b).to_string(),
            _ => format!("%{b:02x}"),
        })
        .collect()
}

/// The file name for `key`: 128 bits from two seeded FNV-1a runs. Two
/// keys of one download meeting is not a case worth a lookup table, and a
/// collision would serve the wrong segment, so the width is generous.
fn file_name(key: &str) -> String {
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(seed, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }
    let a = fnv(0xcbf2_9ce4_8422_2325, key.as_bytes());
    let b = fnv(0x84222325_cbf29ce4, key.as_bytes());
    format!("{a:016x}{b:016x}")
}

/// The time as ISO 8601 UTC, from the system clock.
pub fn iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    iso_from_secs(secs)
}

fn iso_from_secs(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Hinnant's algorithm).
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// The planned download, as [`Inner::admit`] records it.
struct Plan {
    files: Vec<String>,
    estimate: u64,
    total: u32,
    /// Bytes of the plan already on disk (a resumed download).
    stored: u64,
    /// Bytes of an earlier plan's files that could not be deleted: on disk
    /// and against the cap, though the plan does not name them.
    stale: u64,
}

/// The room a download is admitted against, read by the caller.
#[derive(Clone, Copy)]
enum Check {
    /// A new download: the other kinds' bytes and the cap, read after
    /// `ready` (whether the accounting was up before they were read: it
    /// never goes down once up, so a figure read after a true `ready` is a
    /// real one).
    Fresh { other: u64, cap: u64, ready: bool },
    /// A resumed one.
    Resume,
}

enum Admission {
    /// Removed or restarted meanwhile.
    Gone,
    /// The plan was admitted in memory but its record could not be saved.
    Unsaved(std::io::Error),
    Refused {
        refusal: KeepVideoRefusal,
        /// What the other downloads and kinds hold or have reserved.
        used: u64,
        /// The free space as measured.
        free: u64,
        /// What the other running downloads still have to write.
        outstanding: u64,
    },
}

/// What a record holds against the shared cap: the larger of what it has
/// stored and what it expects to take, so a download still under way
/// reserves its whole estimate.
fn reserved(v: &KeptVideo) -> u64 {
    v.bytes().max(v.estimated_bytes)
}

impl State {
    fn dir_of(&self, user: &str, item: &str) -> PathBuf {
        self.root.join(safe_name(user)).join(safe_name(item))
    }

    /// Whether a staging file is still being written in `dir`.
    fn writing(&self, dir: &Path) -> bool {
        self.writers
            .keys()
            .any(|(u, i, _)| self.dir_of(u, i) == *dir)
    }

    fn epoch(&self, dir: &Path) -> u64 {
        self.epochs.get(dir).copied().unwrap_or(0)
    }

    /// Something that can change `dir`'s size happened: measurements taken
    /// before it no longer count.
    fn bump(&mut self, dir: &Path) {
        *self.epochs.entry(dir.to_path_buf()).or_default() += 1;
    }

    /// Forgets a tombstone and everything derived about it.
    fn drop_tombstone(&mut self, dir: &Path) {
        self.tombstones.retain(|(d, _)| d != dir);
        self.measured.remove(dir);
        self.removing.retain(|d| d != dir);
    }

    /// The first folder whose size is not known, and why. Derived from the
    /// facts each time: a stored mark is only the open's findings. A folder
    /// is unknown while a revoked run's writer is running in it with no
    /// run owning the item (whether or not its record is back), while its removal is in flight, and, once
    /// tombstoned, until a measurement that read everything was taken after
    /// the last event that could change it.
    fn unknown_reason(&self) -> Option<(PathBuf, String)> {
        if let Some((dir, why)) = self.unmeasured.first() {
            return Some((dir.clone(), why.clone()));
        }
        if let Some(dir) = self.leftover.first() {
            return Some((
                dir.clone(),
                "a staging file could not be deleted".to_owned(),
            ));
        }
        for (user, item, _) in self.writers.keys() {
            // With another run owning the item, the stale run's bytes are
            // counted under it (`staged`, the owner's estimate); with none
            // (a removal, or a record put back) nothing bounds them.
            // A folder that was deleted has nothing left to bound: the
            // writer's file is an unlinked one.
            let dir = self.dir_of(user, item);
            let kept = find(&self.list, user, item).is_some()
                || self.tombstones.iter().any(|(d, _)| *d == dir);
            if kept && !self.runs.contains_key(&(user.clone(), item.clone())) {
                return Some((
                    self.dir_of(user, item),
                    "a revoked run's writer is still running".to_owned(),
                ));
            }
        }
        for (dir, _) in &self.tombstones {
            if self.removing.contains(dir) {
                return Some((dir.clone(), "being removed, not yet measured".to_owned()));
            }
            if self.measured.get(dir) != Some(&(self.epoch(dir), true)) {
                return Some((
                    dir.clone(),
                    "folder not deleted, not measured whole since it last changed".to_owned(),
                ));
            }
        }
        None
    }

    /// Bytes in the folders of removed downloads that would not delete,
    /// leaving out one a live record has since taken over.
    fn tombstoned(&self) -> u64 {
        self.tombstones
            .iter()
            .chain(self.held.iter())
            .filter(|(dir, _)| {
                !self
                    .list
                    .iter()
                    .any(|v| self.root.join(safe_name(&v.user)).join(safe_name(&v.item)) == *dir)
            })
            .fold(0u64, |sum, (_, bytes)| sum.saturating_add(*bytes))
    }

    /// Why nothing can be admitted now, when a folder's size is unknown.
    fn unknown_folder(&self) -> Option<KeepVideoRefusal> {
        self.unknown_reason().map(|(dir, why)| {
            KeepVideoRefusal::Host(format!(
                "the size of {} is unknown ({why}), so the cap cannot be checked",
                dir.display()
            ))
        })
    }

    /// Every video's reservation but `item`'s own, and the tombstones'.
    fn reserved_by_others(&self, user: &str, item: &str) -> u64 {
        reserved_by_others(&self.list, user, item).saturating_add(self.tombstoned())
    }

    /// What the other running downloads have yet to write: their estimate
    /// less what they have stored whole and what they have staged. The free
    /// space already lacks what is on disk, so only the rest is still to
    /// come out of it.
    fn outstanding(&self, user: &str, item: &str) -> u64 {
        self.list
            .iter()
            .filter(|v| !(v.user == user && v.item == item))
            .filter_map(|v| match v.state {
                KeptState::Downloading { bytes, .. } => {
                    // Only the run that owns the record: its estimate is
                    // what these bytes are taken from.
                    let staged = self
                        .runs
                        .get(&(v.user.clone(), v.item.clone()))
                        .and_then(|run| {
                            self.staged
                                .get(&(v.user.clone(), v.item.clone(), *run))
                                .copied()
                        })
                        .unwrap_or(0);
                    Some(
                        v.estimated_bytes
                            .saturating_sub(bytes.saturating_add(staged)),
                    )
                }
                _ => None,
            })
            .fold(0u64, u64::saturating_add)
    }
}

/// A download's bytes in a staging file, counted in [`State::staged`] until
/// the file is whole, promoted or dropped.
struct Staged<'a> {
    inner: &'a Inner,
    who: (String, String, u64),
    len: u64,
    part: PathBuf,
}

impl Staged<'_> {
    fn add(&mut self, n: u64) {
        self.len = self.len.saturating_add(n);
        let mut st = lock(&self.inner.state);
        let held = st.staged.entry(self.who.clone()).or_default();
        *held = held.saturating_add(n);
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        let mut st = lock(&self.inner.state);
        // A revoked run's staging file goes before its writer is
        // unregistered, so a folder is not bounded while it could be there.
        let revoked = st.runs.get(&(self.who.0.clone(), self.who.1.clone())) != Some(&self.who.2);
        let dir = st.dir_of(&self.who.0, &self.who.1);
        if revoked {
            if let Err(e) = remove_part(&self.part) {
                if e.kind() != std::io::ErrorKind::NotFound && !st.leftover.contains(&dir) {
                    st.leftover.push(dir.clone());
                }
            }
        }
        st.bump(&dir);
        if let Some(n) = st.writers.get_mut(&self.who) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.writers.remove(&self.who);
            }
        }
        if let Some(held) = st.staged.get_mut(&self.who) {
            *held = held.saturating_sub(self.len);
            if *held == 0 {
                st.staged.remove(&self.who);
            }
        }
    }
}

/// Every video's reservation but `item`'s own.
fn reserved_by_others(list: &[KeptVideo], user: &str, item: &str) -> u64 {
    list.iter()
        .filter(|v| !(v.user == user && v.item == item))
        .map(reserved)
        .fold(0u64, u64::saturating_add)
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    /// Tries again to delete the folders of removed downloads that would
    /// not go; the ones that do stop counting. A folder a live record has
    /// taken over is left to that record.
    fn sweep_tombstones(&self) {
        self.settle_leftovers();
        let pending: Vec<PathBuf> = {
            let mut st = lock(&self.state);
            let picked: Vec<PathBuf> = st
                .tombstones
                .iter()
                .map(|(dir, _)| dir.clone())
                .filter(|dir| {
                    !st.removing.contains(dir)
                        && !st.deleting.contains(dir)
                        && !st.writing(dir)
                        && !st
                            .list
                            .iter()
                            .any(|v| self.item_dir(&v.user, &v.item) == *dir)
                })
                .collect();
            st.deleting.extend(picked.iter().cloned());
            picked
        };
        for dir in pending {
            let outcome = (self.env.remove_dir)(&dir);
            {
                let mut st = lock(&self.state);
                st.deleting.retain(|d| *d != dir);
                if st.removing.contains(&dir)
                    || st.writing(&dir)
                    || st
                        .list
                        .iter()
                        .any(|v| self.item_dir(&v.user, &v.item) == dir)
                {
                    // A removal began while the delete ran: it owns the folder.
                    continue;
                }
                if outcome.is_ok() {
                    st.drop_tombstone(&dir);
                    drop(st);
                    emit(
                        false,
                        format!("pharos: video keep tombstone cleared dir={}", dir.display()),
                    );
                    continue;
                }
            }
            if let Err(e) = outcome {
                let bytes = self.measure_tombstone(&dir);
                emit(
                    true,
                    format!(
                        "pharos: video keep tombstone kept dir={} bytes={bytes} error={e}",
                        dir.display()
                    ),
                );
            }
        }
    }

    /// Resolves folders where a staging file could not be deleted: with no
    /// writer in one, the files are deleted if no run owns the item, and
    /// the folder is then measured whole (staging files included); read in
    /// full, its bytes are taken into the figure that counts for it (the
    /// record's reservation, saved, or the tombstone's) and the fact clears.
    /// A folder that cannot be read whole, or whose save failed, stays
    /// unknown. One settlement per folder at a time.
    fn settle_leftovers(&self) {
        let dirs: Vec<PathBuf> = {
            let st = lock(&self.state);
            st.leftover
                .iter()
                .filter(|d| !st.writing(d) && !st.removing.contains(d) && !st.settling.contains(d))
                .cloned()
                .collect()
        };
        for dir in dirs {
            let claimed = {
                let mut st = lock(&self.state);
                if st.settling.contains(&dir) || st.writing(&dir) || st.removing.contains(&dir) {
                    None
                } else {
                    st.settling.push(dir.clone());
                    let owned = st.runs.keys().any(|(u, i)| st.dir_of(u, i) == dir);
                    Some((st.epoch(&dir), owned))
                }
            };
            let Some((epoch, owned)) = claimed else {
                continue;
            };
            self.settle_claimed(&dir, epoch, owned);
            lock(&self.state).settling.retain(|d| *d != dir);
        }
    }

    /// Whether `dir` is still as a settlement that began at `epoch` found
    /// it: no writer, no removal, no move of its epoch.
    fn unchanged_since(st: &State, dir: &Path, epoch: u64) -> bool {
        st.epoch(dir) == epoch && !st.writing(dir) && !st.removing.iter().any(|d| d == dir)
    }

    fn settle_claimed(&self, dir: &Path, epoch: u64, owned: bool) {
        #[cfg(test)]
        tests::before_settle_drop();
        if !owned {
            self.drop_parts_settling(dir, epoch);
        }
        let measured = dir_size(dir);
        #[cfg(test)]
        tests::during_measure();
        let (record, saved_floor) = {
            let mut st = lock(&self.state);
            if !measured.exact || !Self::unchanged_since(&st, dir, epoch) {
                return;
            }
            let record = st
                .list
                .iter()
                .position(|v| st.dir_of(&v.user, &v.item) == dir);
            let tomb = st.tombstones.iter().position(|(d, _)| d == dir);
            let mut ident = None;
            if let Some(i) = record {
                let v = &mut st.list[i];
                v.estimated_bytes = v.estimated_bytes.max(measured.bytes);
                ident = Some((v.user.clone(), v.item.clone()));
            }
            if let Some(i) = tomb {
                st.tombstones[i].1 = st.tombstones[i].1.max(measured.bytes);
            }
            (ident, measured.bytes)
        };
        // The larger reservation must be on disk before the fact clears: a
        // restart would otherwise read the old, smaller one.
        if let Some((user, item)) = &record {
            if self.save_record(user, item).is_err() {
                return;
            }
        }
        let mut st = lock(&self.state);
        if !Self::unchanged_since(&st, dir, epoch) {
            return;
        }
        st.leftover.retain(|d| d != dir);
        drop(st);
        emit(
            false,
            format!(
                "pharos: video keep leftover settled dir={} bytes={saved_floor}",
                dir.display()
            ),
        );
    }

    /// Deletes the staging files in `dir`, each under the state lock after
    /// checking again that no run owns the folder and nothing moved its
    /// epoch since the settlement began: a retry's new file is never taken.
    fn drop_parts_settling(&self, dir: &Path, epoch: u64) -> bool {
        let (paths, mut clean) = part_paths(dir, ".part");
        for path in paths {
            let st = lock(&self.state);
            let owned = st.runs.keys().any(|(u, i)| st.dir_of(u, i) == dir);
            if owned || !Self::unchanged_since(&st, dir, epoch) {
                return false;
            }
            match remove_part(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => clean = false,
            }
        }
        clean
    }

    /// Measures a tombstoned folder and records the figure, unless something
    /// that could change it happened meanwhile: the walk is made outside the
    /// lock, and its result is taken only if the folder's epoch has not
    /// moved and no removal or writer owns the folder; otherwise it is made
    /// again. With a writer or a removal in flight nothing is measured (the
    /// folder stays unknown): the party that ends last measures. Returns the
    /// figure counted for the folder. An unreadable folder keeps the figure
    /// it had.
    fn measure_tombstone(&self, dir: &Path) -> u64 {
        let mut figure = 0;
        for _ in 0..4 {
            let epoch = {
                let st = lock(&self.state);
                let Some((_, bytes)) = st.tombstones.iter().find(|(d, _)| d == dir) else {
                    return 0;
                };
                figure = *bytes;
                if st.removing.iter().any(|d| d == dir) || st.writing(dir) {
                    return figure;
                }
                st.epoch(dir)
            };
            let measured = dir_size(dir);
            #[cfg(test)]
            tests::during_measure();
            let mut st = lock(&self.state);
            if st.removing.iter().any(|d| d == dir) || st.writing(dir) || st.epoch(dir) != epoch {
                continue;
            }
            let Some(t) = st.tombstones.iter_mut().find(|(d, _)| d == dir) else {
                return 0;
            };
            t.1 = measured.counted(t.1);
            figure = t.1;
            st.measured
                .insert(dir.to_path_buf(), (epoch, measured.exact));
            return figure;
        }
        figure
    }

    /// Puts back the record of a removal that could not take its manifest
    /// off the disk, with the files it still holds indexed.
    fn keep_after_failed_unlink(&self, mut record: KeptVideo, dir: &Path, e: &std::io::Error) {
        let names = stored_names(dir);
        #[cfg(test)]
        tests::at_unlink_failure();
        let mut st = lock(&self.state);
        // The record comes back and the removal's tombstone, mark and
        // in-removal entry go in this one hold: no admission sees neither.
        st.drop_tombstone(dir);
        for key in &record.files {
            if let Some(len) = names.get(&file_name(key)) {
                st.index.insert(
                    (record.user.clone(), key.clone()),
                    (self.file_path(&record.user, &record.item, key), *len),
                );
            }
        }
        // Nothing runs it any more.
        if matches!(record.state, KeptState::Downloading { .. }) {
            // A Failed record reports its estimate as its bytes: what the
            // download had written, or the folder holds, must not drop out
            // of what it reserves in the change.
            // The whole folder, staging files included.
            let on_disk = dir_size(dir).bytes;
            record.estimated_bytes = reserved(&record).max(on_disk);
            record.state = KeptState::Failed {
                reason: format!("could not be removed: {e}"),
            };
        }
        emit(
            true,
            format!(
                "pharos: video keep remove item={} verdict=not_removed manifest={} error={e}",
                record.item,
                dir.join("manifest.json").display()
            ),
        );
        insert_front(&mut st.list, record);
    }

    fn item_dir(&self, user: &str, item: &str) -> PathBuf {
        self.root.join(safe_name(user)).join(safe_name(item))
    }

    fn file_path(&self, user: &str, item: &str, key: &str) -> PathBuf {
        self.item_dir(user, item).join("files").join(file_name(key))
    }

    /// Writes the current record of `item` beside its files, whole or not
    /// at all. Saves are serialised and write what the record holds when
    /// their turn comes, never an older copy, and a record that has since
    /// been removed is not written back: [`DiskVideoStore::remove`] holds
    /// the same lock while it deletes the folder. A record that is gone has
    /// nothing to write; a write that fails is logged and returned.
    fn save_record(&self, user: &str, item: &str) -> std::io::Result<()> {
        let _saving = self.saves.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(video) = find(&lock(&self.state).list, user, item).cloned() else {
            return Ok(());
        };
        self.write_manifest(&video)
    }

    /// Writes `video`'s manifest, whole or not at all, logging a failure.
    /// The caller holds the `saves` lock.
    fn write_manifest(&self, video: &KeptVideo) -> std::io::Result<()> {
        #[cfg(test)]
        tests::during_save(video);
        let dir = self.item_dir(&video.user, &video.item);
        let result = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let text = serde_json::to_vec(&video).map_err(std::io::Error::other)?;
            let tmp = dir.join("manifest.json.part");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, dir.join("manifest.json"))
        })();
        if let Err(e) = &result {
            emit(
                true,
                format!(
                    "pharos: video record not saved item={} dir={} error={e}",
                    video.item,
                    dir.display()
                ),
            );
        }
        result
    }

    /// Changes the record of `item` and saves it: `Ok(false)` when it is
    /// gone. When the save fails the change is kept in memory, not rolled
    /// back, and the error is returned: memory is what the UI shows, the
    /// next successful save writes it, and undoing a play position or a
    /// progress figure over a transient disk fault would only lose it.
    fn update(
        &self,
        user: &str,
        item: &str,
        f: impl FnOnce(&mut KeptVideo),
    ) -> std::io::Result<bool> {
        {
            let mut st = lock(&self.state);
            let Some(v) = st
                .list
                .iter_mut()
                .find(|v| v.user == user && v.item == item)
            else {
                return Ok(false);
            };
            f(v);
        }
        self.save_record(user, item).map(|()| true)
    }

    /// [`Inner::update`] for the run that owns the download: the run id is
    /// checked under the same lock as the change, so a run that was removed
    /// or restarted since cannot touch the record of the one that replaced
    /// it. `Ok(false)` when `run` no longer owns `item`; an error when the
    /// change could not be saved (it stays in memory, as in `update`).
    fn update_run(
        &self,
        user: &str,
        item: &str,
        run: u64,
        f: impl FnOnce(&mut KeptVideo),
    ) -> std::io::Result<bool> {
        {
            let mut st = lock(&self.state);
            if st.runs.get(&(user.to_owned(), item.to_owned())) != Some(&run) {
                return Ok(false);
            }
            let Some(v) = st
                .list
                .iter_mut()
                .find(|v| v.user == user && v.item == item)
            else {
                return Ok(false);
            };
            f(v);
        }
        self.save_record(user, item).map(|()| true)
    }

    /// [`Inner::update_run`] for a change that must not be seen before it
    /// is on disk (a download becoming Ready): the changed record is saved
    /// first, and only then published in memory, under the ownership check
    /// and with the `saves` lock held throughout so no other save or
    /// removal comes between. A failed save leaves memory untouched.
    fn update_run_saved(
        &self,
        user: &str,
        item: &str,
        run: u64,
        f: impl Fn(&mut KeptVideo),
    ) -> std::io::Result<bool> {
        let _saving = self.saves.lock().unwrap_or_else(PoisonError::into_inner);
        let owns = |st: &State| st.runs.get(&(user.to_owned(), item.to_owned())) == Some(&run);
        let candidate = {
            let st = lock(&self.state);
            if !owns(&st) {
                return Ok(false);
            }
            let Some(mut v) = find(&st.list, user, item).cloned() else {
                return Ok(false);
            };
            f(&mut v);
            v
        };
        self.write_manifest(&candidate)?;
        let mut st = lock(&self.state);
        if !owns(&st) {
            return Ok(false);
        }
        let Some(v) = st
            .list
            .iter_mut()
            .find(|v| v.user == user && v.item == item)
        else {
            return Ok(false);
        };
        f(v);
        Ok(true)
    }

    /// Deletes a staging file; one that will not go is recorded as a
    /// leftover of its folder, which is unknown until a sweep resolves it.
    fn remove_staging(&self, part: &Path) {
        let Err(e) = remove_part(part) else { return };
        if e.kind() == std::io::ErrorKind::NotFound {
            return;
        }
        if let Some(dir) = part.parent().and_then(Path::parent) {
            let mut st = lock(&self.state);
            if !st.leftover.iter().any(|d| d == dir) {
                st.leftover.push(dir.to_path_buf());
            }
        }
    }

    fn owns(&self, user: &str, item: &str, run: u64) -> bool {
        lock(&self.state)
            .runs
            .get(&(user.to_owned(), item.to_owned()))
            == Some(&run)
    }

    fn wanted(&self, user: &str, item: &str, run: u64) -> bool {
        let st = lock(&self.state);
        st.runs.get(&(user.to_owned(), item.to_owned())) == Some(&run)
            && find(&st.list, user, item)
                .is_some_and(|v| matches!(v.state, KeptState::Downloading { .. }))
    }

    /// Admits `run`'s download: the plan is recorded and its estimate
    /// reserved in one step under the state lock, after the room it needs
    /// is checked beside every other download's reservation. Checking and
    /// reserving together is what stops two downloads from each fitting
    /// alone, against the cap (`Check::Fresh`) and against the free space,
    /// where each other running download still has its unwritten bytes to
    /// take from it.
    fn admit(
        &self,
        user: &str,
        item: &str,
        run: u64,
        plan: &Plan,
        check: Check,
    ) -> Result<(), Admission> {
        {
            let mut st = lock(&self.state);
            if st.runs.get(&(user.to_owned(), item.to_owned())) != Some(&run) {
                return Err(Admission::Gone);
            }
            // What the running downloads still owe is read before the free
            // space is measured: a write landing in between lowers free, and
            // the stale (higher) owed figure only makes the check stricter.
            // The other order counts that write as room.
            let outstanding = st
                .outstanding(user, item)
                .saturating_add(self.env.budget.other_outstanding_bytes());
            if let Some(refusal) = st.unknown_folder() {
                return Err(Admission::Refused {
                    refusal,
                    used: 0,
                    free: 0,
                    outstanding,
                });
            }
            let free = self.env.budget.free_bytes();
            let verdict = match check {
                // A new plan is checked against the shared cap, so it needs
                // the other kinds' figures: a download restarted with no
                // plan (and none of its files) is fresh too, and must pass
                // the gate `start` applies.
                Check::Fresh { ready: false, .. } => Err((
                    KeepVideoRefusal::Host(
                        "the music cache is not up yet, so the shared cap cannot be checked"
                            .to_owned(),
                    ),
                    0,
                    free,
                )),
                Check::Fresh { other, cap, .. } => {
                    let used = st.reserved_by_others(user, item).saturating_add(other);
                    fits(
                        plan.estimate,
                        used.saturating_add(plan.stale),
                        cap,
                        free.saturating_sub(outstanding),
                    )
                    .map_err(|r| (r, used, free))
                }
                // The cap was met when the download began and its stored
                // bytes are counted; what is left needs room, and 0 free
                // (an unreadable measure) is no room.
                Check::Resume => fits(
                    plan.estimate.saturating_sub(plan.stored),
                    0,
                    u64::MAX,
                    free.saturating_sub(outstanding),
                )
                .map_err(|r| (r, 0, free)),
            };
            if let Err((refusal, used, free)) = verdict {
                return Err(Admission::Refused {
                    refusal,
                    used,
                    free,
                    outstanding,
                });
            }
            let Some(v) = st
                .list
                .iter_mut()
                .find(|v| v.user == user && v.item == item)
            else {
                return Err(Admission::Gone);
            };
            v.files.clone_from(&plan.files);
            // The reservation holds what the plan will take and what an
            // earlier plan still leaves on disk.
            v.estimated_bytes = plan.estimate.saturating_add(plan.stale);
            v.state = KeptState::Downloading {
                done: 0,
                total: plan.total,
                bytes: plan.stored,
            };
        }
        self.save_record(user, item).map_err(Admission::Unsaved)
    }

    /// Indexes a whole file for `run`; false when `run` no longer owns the
    /// download, which indexes nothing.
    fn record_file(
        &self,
        user: &str,
        item: &str,
        run: u64,
        key: &str,
        path: PathBuf,
        len: u64,
    ) -> bool {
        let mut st = lock(&self.state);
        if st.runs.get(&(user.to_owned(), item.to_owned())) != Some(&run) {
            return false;
        }
        st.index
            .insert((user.to_owned(), key.to_owned()), (path, len));
        true
    }

    /// Records progress; the record is written at most once a second
    /// unless `force`.
    fn publish(
        &self,
        user: &str,
        item: &str,
        run: u64,
        (done, total, bytes): (usize, u32, u64),
        force: bool,
    ) {
        let done = u32::try_from(done).unwrap_or(u32::MAX);
        let due = {
            let mut st = lock(&self.state);
            let now = Instant::now();
            let due = force
                || st.last_saved.map_or(true, |t| {
                    progress_due(0.0, now.duration_since(t).as_secs_f64() * 1_000.0)
                });
            if due {
                st.last_saved = Some(now);
            }
            due
        };
        if due {
            // Progress only: a failed save is logged by `save_record`, and
            // the next one (or the terminal state's) writes it again.
            let _ = self.update_run(user, item, run, |v| {
                v.state = KeptState::Downloading { done, total, bytes };
            });
        } else {
            let mut st = lock(&self.state);
            if st.runs.get(&(user.to_owned(), item.to_owned())) == Some(&run) {
                if let Some(v) = st
                    .list
                    .iter_mut()
                    .find(|v| v.user == user && v.item == item)
                {
                    v.state = KeptState::Downloading { done, total, bytes };
                }
            }
        }
    }
}

/// Fetches `key` to its file, retrying with backoff while a retry can
/// mend the failure, and stops once the download has been removed.
async fn fetch_file(
    inner: &Inner,
    user: &str,
    item: &str,
    run: u64,
    key: &str,
) -> Result<u64, Stop> {
    let path = inner.file_path(user, item, key);
    let mut attempt = 0u32;
    loop {
        if !inner.wanted(user, item, run) {
            return Err(Stop::Gone);
        }
        let err = match fetch_once(inner, user, item, run, key, &path).await {
            Ok(len) => {
                return if inner.record_file(user, item, run, key, path, len) {
                    Ok(len)
                } else {
                    Err(Stop::Gone)
                };
            }
            Err(e) => e,
        };
        let failure = match err {
            FetchOnce::Store(reason) => {
                emit(
                    true,
                    format!(
                        "pharos: video keep store failed item={item} url_path={} reason={reason}",
                        url_path(key)
                    ),
                );
                return if inner.wanted(user, item, run) {
                    Err(Stop::Failed(reason))
                } else {
                    Err(Stop::Gone)
                };
            }
            FetchOnce::Gone => return Err(Stop::Gone),
            FetchOnce::Fetch(e) => e,
        };
        attempt += 1;
        emit(
            true,
            format!(
                "pharos: video keep file failed item={item} url_path={} status={} attempt={attempt}",
                url_path(key),
                failure.status.map_or_else(|| "none".to_owned(), |s| s.to_string()),
            ),
        );
        match (is_retryable(failure.status), retry_delay_secs(attempt)) {
            (true, Some(secs)) => (inner.env.sleep)(secs).await,
            _ => {
                return Err(Stop::Failed(format!(
                    "{} answered {failure}",
                    url_path(key)
                )))
            }
        }
    }
}

enum FetchOnce {
    Fetch(FetchError),
    Store(String),
    /// The download was removed or restarted while this run was fetching.
    Gone,
}

/// Waits for a chunk's write to complete. In a test the write can be held
/// pending first, to show nothing is counted as staged meanwhile.
async fn flush_chunk(file: &mut tokio::fs::File, item: &str) -> std::io::Result<()> {
    #[cfg(test)]
    tests::hold_write(item).await;
    #[cfg(not(test))]
    let _ = item;
    file.flush().await
}

/// One attempt: the body into `<path>.part`, then renamed into place.
///
/// The body is staged in a file named for `run` (`<name>.<run>.part`), so
/// two runs of one item never share a staging file, and it is promoted only
/// while `run` still owns the download: the check and the rename happen
/// under the state lock, so a removed or restarted run cannot put its bytes
/// in place.
async fn fetch_once(
    inner: &Inner,
    user: &str,
    item: &str,
    run: u64,
    key: &str,
    path: &Path,
) -> Result<u64, FetchOnce> {
    let store_err = |what: &str, e: std::io::Error| {
        FetchOnce::Store(format!(
            "could not {what} {}: {e}",
            path.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        ))
    };
    let mut body = inner
        .env
        .fetcher
        .open(key)
        .await
        .map_err(FetchOnce::Fetch)?;
    let part = path.with_extension(format!("{run}.part"));
    // The folder and the staging file are created in one step, with no
    // await to be cancelled in, and only while this run still owns the
    // item: a removal revokes the run under the same lock, so an old run
    // cannot bring the folder or a file back after a sweep measured it.
    // (A write to a handle opened before the revocation lands in an
    // unlinked file.)
    let who = (user.to_owned(), item.to_owned(), run);
    let (mut file, mut staged) = {
        let mut st = lock(&inner.state);
        if st.runs.get(&(user.to_owned(), item.to_owned())) != Some(&run) {
            return Err(FetchOnce::Gone);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| store_err("create the folder for", e))?;
        }
        let file = std::fs::File::create(&part)
            .map(tokio::fs::File::from_std)
            .map_err(|e| store_err("create", e))?;
        // Registered as a writer with the file, under the same hold.
        *st.writers.entry(who.clone()).or_default() += 1;
        let dir = st.dir_of(user, item);
        st.bump(&dir);
        (
            file,
            Staged {
                inner,
                who,
                len: 0,
                part: part.clone(),
            },
        )
    };
    let mut len = 0u64;
    loop {
        match body.next_chunk().await {
            Ok(Some(chunk)) => {
                // A run a removal or restart has revoked writes no more.
                if !inner.owns(user, item, run) {
                    drop(file);
                    return Err(FetchOnce::Gone);
                }
                file.write_all(&chunk)
                    .await
                    .map_err(|e| store_err("write", e))?;
                // `write_all` can return while tokio's write is still
                // pending, and the free-space reading does not lack those
                // bytes yet. They count as staged (and so stop being owed)
                // only once the write has completed.
                flush_chunk(&mut file, item)
                    .await
                    .map_err(|e| store_err("write", e))?;
                len = len.saturating_add(chunk.len() as u64);
                staged.add(chunk.len() as u64);
            }
            Ok(None) => break,
            Err(e) => {
                drop(file);
                inner.remove_staging(&part);
                return Err(FetchOnce::Fetch(e));
            }
        }
    }
    file.flush().await.map_err(|e| store_err("write", e))?;
    drop(file);
    let promoted = {
        let st = lock(&inner.state);
        let owns = st.runs.get(&(user.to_owned(), item.to_owned())) == Some(&run)
            && find(&st.list, user, item)
                .is_some_and(|v| matches!(v.state, KeptState::Downloading { .. }));
        if owns {
            Some(std::fs::rename(&part, path))
        } else {
            None
        }
    };
    match promoted {
        Some(Ok(())) => Ok(len),
        Some(Err(e)) => Err(store_err("finish", e)),
        None => {
            inner.remove_staging(&part);
            Err(FetchOnce::Gone)
        }
    }
}

/// A playlist's text: the stored file, or fetched and stored first.
async fn playlist(
    inner: &Inner,
    user: &str,
    item: &str,
    run: u64,
    key: &str,
) -> Result<String, Stop> {
    let path = inner.file_path(user, item, key);
    if tokio::fs::metadata(&path).await.is_err() {
        fetch_file(inner, user, item, run, key).await?;
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| Stop::Failed(format!("could not read {}: {e}", url_path(key))))?;
    String::from_utf8(bytes).map_err(|_| Stop::Failed(format!("{} is not text", url_path(key))))
}

/// The files under `dir/files` as (name, length).
fn stored_names(dir: &Path) -> HashMap<String, u64> {
    stored_names_checked(dir).0
}

/// The files under `dir/files` as (name, length), and whether the walk
/// completed: false when the folder, an entry or a file's length could not
/// be read (anything but being gone), so the list may be short.
fn stored_names_checked(dir: &Path) -> (HashMap<String, u64>, bool) {
    let read = match std::fs::read_dir(dir.join("files")) {
        Ok(read) => read,
        Err(e) => return (HashMap::new(), e.kind() == std::io::ErrorKind::NotFound),
    };
    let mut complete = true;
    let mut names = HashMap::new();
    for entry in read {
        let Ok(e) = entry else {
            complete = false;
            continue;
        };
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".part") {
            continue;
        }
        match e.metadata() {
            Ok(m) => {
                names.insert(name, m.len());
            }
            Err(err) => complete &= err.kind() == std::io::ErrorKind::NotFound,
        }
    }
    (names, complete)
}

/// A folder whose record cannot be read is left alone, never deleted. Its
/// bytes (as far as they can be read) count against the cap, and since the
/// record's own reservation is lost, nothing is admitted while it stays.
fn hold_folder(
    dir: &Path,
    why: &str,
    held: &mut Vec<(PathBuf, u64)>,
    unmeasured: &mut Vec<(PathBuf, String)>,
) {
    let m = dir_size(dir);
    emit(
        true,
        format!(
            "pharos: video folder held dir={} reason={why} bytes={} exact={} admission=blocked",
            dir.display(),
            m.bytes,
            m.exact
        ),
    );
    held.push((dir.to_path_buf(), m.bytes));
    unmeasured.push((dir.to_path_buf(), why.to_owned()));
}

/// What a walk of a folder found.
struct Measured {
    bytes: u64,
    /// False when something under it could not be read (anything but being
    /// gone): the bytes are then a lower bound.
    exact: bool,
}

impl Measured {
    /// The figure to count against the cap: the bytes found, or, when the
    /// walk was cut short, no less than `floor` (what was known before).
    fn counted(&self, floor: u64) -> u64 {
        if self.exact {
            self.bytes
        } else {
            self.bytes.max(floor)
        }
    }
}

/// Every byte of every file under `dir`, whatever it is. A folder that is
/// gone holds nothing; one that cannot be read is not counted as empty.
fn dir_size(dir: &Path) -> Measured {
    #[cfg(test)]
    if tests::UNREADABLE.with(|u| u.borrow().iter().any(|p| p == dir)) {
        return Measured {
            bytes: 0,
            exact: false,
        };
    }
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) => {
            return Measured {
                bytes: 0,
                exact: e.kind() == std::io::ErrorKind::NotFound,
            };
        }
    };
    let mut total = Measured {
        bytes: 0,
        exact: true,
    };
    for entry in read {
        let part = match entry {
            Err(_) => Measured {
                bytes: 0,
                exact: false,
            },
            Ok(e) => match e.metadata() {
                Ok(m) if m.is_dir() => dir_size(&e.path()),
                Ok(m) => Measured {
                    bytes: m.len(),
                    exact: true,
                },
                Err(err) => Measured {
                    bytes: 0,
                    exact: err.kind() == std::io::ErrorKind::NotFound,
                },
            },
        };
        total.bytes = total.bytes.saturating_add(part.bytes);
        total.exact &= part.exact;
    }
    total
}

/// Bytes of whole files on disk under `dir`, whatever plan they belong to.
/// Not exact when the walk could not complete: the bytes are then a lower
/// bound, and must not be taken as what the folder holds.
fn disk_bytes(dir: &Path) -> Measured {
    #[cfg(test)]
    if tests::FAIL_DISK_WALK.with(std::cell::Cell::get) {
        return Measured {
            bytes: 0,
            exact: false,
        };
    }
    let (names, exact) = stored_names_checked(dir);
    Measured {
        bytes: names
            .values()
            .fold(0u64, |sum, len| sum.saturating_add(*len)),
        exact,
    }
}

/// Deletes the stored files of `item` that `plan` does not name, and
/// forgets them: a retry that planned afresh (a new play session names
/// every file anew) must not leave the old plan's files on the disk.
/// Returns the bytes of unplanned files that could not be deleted, which
/// still count against the cap.
fn prune_unplanned(inner: &Inner, user: &str, item: &str, plan: &[String]) -> u64 {
    let dir = inner.item_dir(user, item);
    let keep: HashSet<String> = plan.iter().map(|k| file_name(k)).collect();
    let files = dir.join("files");
    let (mut removed, mut bytes, mut left) = (0usize, 0u64, 0u64);
    for (name, len) in stored_names(&dir) {
        if keep.contains(&name) {
            continue;
        }
        if std::fs::remove_file(files.join(&name)).is_ok() {
            removed += 1;
            bytes = bytes.saturating_add(len);
        } else {
            left = left.saturating_add(len);
        }
    }
    lock(&inner.state).index.retain(|_, (path, _)| {
        path.parent() != Some(files.as_path()) || {
            path.file_name()
                .is_some_and(|n| keep.contains(n.to_string_lossy().as_ref()))
        }
    });
    if removed > 0 {
        emit(
            false,
            format!("pharos: video keep pruned item={item} files={removed} bytes={bytes}"),
        );
    }
    if left > 0 {
        emit(
            true,
            format!("pharos: video keep prune left item={item} bytes={left}"),
        );
    }
    left
}

/// One planned file; the sliver of a last segment may go without. The
/// answer is the key and its stored length, `None` when it was skipped.
async fn fetch_planned(
    inner: &Inner,
    user: &str,
    item: &str,
    run: u64,
    key: String,
    short_tails: &HashSet<String>,
) -> Result<(String, Option<u64>), Stop> {
    match fetch_file(inner, user, item, run, &key).await {
        Ok(len) => Ok((key, Some(len))),
        // B1476: the sliver of a last segment can fail to encode; the
        // copy plays without it, ending a frame early.
        Err(Stop::Failed(reason)) if short_tails.contains(&key) => {
            emit(
                true,
                format!(
                    "pharos: video keep skipped short tail item={item} url_path={} reason={reason}",
                    url_path(&key)
                ),
            );
            Ok((key, None))
        }
        Err(e) => Err(e),
    }
}

async fn download(inner: &Arc<Inner>, user: &str, item: &str, run: u64) -> Result<Finished, Stop> {
    let record = find(&lock(&inner.state).list, user, item)
        .cloned()
        .ok_or(Stop::Gone)?;
    let resume = !record.files.is_empty();
    let master = request_key(&record.master_url);

    let text = playlist(inner, user, item, run, &master).await?;
    let plan = plan_master(&format!("{PLAN_ORIGIN}{master}"), &text)
        .map_err(|e| Stop::Failed(format!("master playlist not understood: {e}")))?;
    let mut stored = vec![master.clone()];
    let mut files: Vec<String> = Vec::new();
    let mut duration_ms = 0u64;
    let mut short_tails: HashSet<String> = HashSet::new();
    for url in &plan.media_playlists {
        let key = request_key(url);
        let text = playlist(inner, user, item, run, &key).await?;
        let media = plan_media(url, &text)
            .map_err(|e| Stop::Failed(format!("{} not understood: {e}", url_path(&key))))?;
        stored.push(key);
        duration_ms = duration_ms.max(media.duration_ms);
        short_tails.extend(media.short_tail.iter().map(|u| request_key(u)));
        files.extend(media.files.iter().map(|u| request_key(u)));
    }
    let mut all = stored.clone();
    all.extend(files);
    all.extend(record.subtitles.iter().map(|s| request_key(&s.url)));
    let mut seen = HashSet::new();
    all.retain(|f| seen.insert(f.clone()));

    let estimate = estimate_bytes(plan.bandwidth_bps, duration_ms);
    emit(
        false,
        format!(
            "pharos: video keep start item={item} quality={} files={} estimate={estimate} resume={resume}",
            record.quality_bps,
            all.len()
        ),
    );
    // Files of an earlier plan go first, so the admission below counts any
    // that will not (they stay on disk, and against the cap, meanwhile).
    if !inner.wanted(user, item, run) {
        return Err(Stop::Gone);
    }
    let stale = prune_unplanned(inner, user, item, &all);
    // What is on disk already, before the plan is admitted: a resumed
    // download is admitted for what is left.
    let names = stored_names(&inner.item_dir(user, item));
    let mut have = HashSet::new();
    let mut stored_bytes = 0u64;
    for key in &all {
        if let Some(len) = names.get(&file_name(key)) {
            have.insert(key.clone());
            stored_bytes = stored_bytes.saturating_add(*len);
        }
    }
    let plan = Plan {
        files: all.clone(),
        estimate,
        total: u32::try_from(all.len()).unwrap_or(u32::MAX),
        stored: if resume { stored_bytes } else { 0 },
        stale,
    };
    let total = plan.total;
    // The room is read and the plan admitted under the host's shared
    // admission lock, taken before the store's own lock (the music cache's
    // lock is never awaited while the store's is held).
    let (check, admitted) = {
        let _admission = inner.env.budget.admission_guard();
        let check = if resume {
            Check::Resume
        } else {
            // Readiness first, then the figures: a library published in
            // between leaves the check refusing, never passing on the
            // default cap and no music.
            let ready = inner.env.budget.accounting_ready();
            Check::Fresh {
                other: inner.env.budget.other_used_bytes(),
                cap: inner.env.budget.cap_bytes(),
                ready,
            }
        };
        (check, inner.admit(user, item, run, &plan, check))
    };
    match admitted {
        Ok(()) => {}
        Err(Admission::Gone) => return Err(Stop::Gone),
        // A plan the disk does not hold would be lost at a restart: the
        // download stops with the reason rather than fetching on.
        Err(Admission::Unsaved(e)) => {
            return Err(Stop::Failed(format!(
                "the plan's record could not be saved: {e}"
            )));
        }
        Err(Admission::Refused {
            refusal,
            used,
            free,
            outstanding,
        }) => {
            let detail = match check {
                Check::Fresh { cap, .. } => format!("estimate={estimate} used={used} cap={cap}"),
                Check::Resume => format!(
                    "resume=true remaining={} stored={stored_bytes}",
                    estimate.saturating_sub(stored_bytes)
                ),
            };
            emit(
                true,
                format!(
                    "pharos: video keep refused item={item} reason={refusal} {detail} free={free} outstanding={outstanding}"
                ),
            );
            // A new download keeps nothing it fetched to plan; a resumed
            // one keeps its files for a later retry.
            if !resume {
                drop_refused_files(inner, user, item, run, &stored);
            }
            return Err(Stop::Refused(refusal.to_string()));
        }
    }
    let mut bytes = 0u64;
    for key in &have {
        if let Some(len) = names.get(&file_name(key)) {
            bytes = bytes.saturating_add(*len);
            inner.record_file(user, item, run, key, inner.file_path(user, item, key), *len);
        }
    }
    let todo = missing_files(&all, &have);
    let mut done = all.len() - todo.len();
    inner.publish(user, item, run, (done, total, bytes), true);

    let short_tails = &short_tails;
    let mut results = stream::iter(todo)
        .map(|key| fetch_planned(inner, user, item, run, key, short_tails))
        .buffer_unordered(CONCURRENCY);
    let mut skipped = Vec::new();
    while let Some(result) = results.next().await {
        let (key, len) = result?;
        match len {
            Some(len) => bytes = bytes.saturating_add(len),
            None => skipped.push(key),
        }
        done += 1;
        inner.publish(user, item, run, (done, total, bytes), false);
    }
    Ok(Finished {
        bytes,
        files: all.len() - skipped.len(),
        skipped,
    })
}

/// What a download that reached its end leaves.
struct Finished {
    bytes: u64,
    /// Files stored, the skipped short tail not counted.
    files: usize,
    /// Planned files that were skipped: not stored, so not in the record.
    skipped: Vec<String>,
}

async fn run(inner: Arc<Inner>, user: String, item: String, run: u64) {
    let started = Instant::now();
    let outcome = download(&inner, &user, &item, run).await;
    let ms = started.elapsed().as_millis();
    let superseded = |what: &str| {
        emit(
            false,
            format!("pharos: video keep stopped item={item} reason={what} ms={ms}"),
        );
    };
    // What the item's folder holds now, whatever plan the files are of.
    let disk = disk_bytes(&inner.item_dir(&user, &item));
    let held = disk.bytes;
    // The record's figure for the folder: what was found, or, when the walk
    // fell short (a transient EMFILE reads as nothing), no less than what
    // the record held before. A short walk also leaves the folder unknown
    // until a sweep measures it whole (below).
    // `completed` is what the record held as a download before it became
    // terminal: a Ready or Failed record reports its estimate as its bytes,
    // so it must be read before the state changes.
    let figure = |v: &KeptVideo, completed: u64| disk.counted(v.estimated_bytes.max(completed));
    // A walk that fell short leaves the folder unknown from before the
    // terminal record is published: an admission must not see the low
    // figure first.
    if !disk.exact {
        let dir = inner.item_dir(&user, &item);
        let mut st = lock(&inner.state);
        if !st.leftover.contains(&dir) {
            st.leftover.push(dir);
        }
    }
    match outcome {
        Ok(Finished {
            bytes,
            files,
            skipped,
        }) => {
            let saved = inner.update_run_saved(&user, &item, run, |v| {
                // A skipped file is not on disk: left in the record, a
                // reopen would count it missing and resume the copy.
                v.files.retain(|k| !skipped.contains(k));
                let completed = v.bytes();
                v.state = KeptState::Ready;
                // What the folder holds, a file a failed prune could not
                // delete included: the record reports all storage used.
                v.estimated_bytes = figure(v, completed);
            });
            match saved {
                Ok(true) => emit(
                    false,
                    format!(
                        "pharos: video keep ready item={item} bytes={bytes} files={files} ms={ms}"
                    ),
                ),
                Ok(false) => superseded("superseded"),
                Err(e) => {
                    // Ready that is not on disk would resume at the next
                    // open: it is reported as what it is, a failure, and
                    // the memory and the disk then tell the same story
                    // (the files stay; a retry finds them).
                    let reason = format!("finished but its record could not be saved: {e}");
                    let _ = inner.update_run(&user, &item, run, |v| {
                        // A Failed record reports its estimate as its bytes:
                        // what the folder holds must not drop out of it.
                        v.estimated_bytes = held.max(v.bytes()).max(v.estimated_bytes);
                        v.state = KeptState::Failed {
                            reason: reason.clone(),
                        };
                    });
                    emit(
                        true,
                        format!(
                            "pharos: video keep failed item={item} reason={reason} files={files} bytes={bytes} ms={ms}"
                        ),
                    );
                }
            }
        }
        Err(Stop::Gone) => superseded("removed"),
        Err(Stop::Refused(reason) | Stop::Failed(reason)) => {
            let (mut files, mut bytes) = (0, 0);
            let applied = inner.update_run(&user, &item, run, |v| {
                // Not durable when the save fails (logged by `save_record`):
                // the disk then holds the earlier Downloading record, which
                // resumes at the next open.
                if let KeptState::Downloading { done, .. } = v.state {
                    files = done;
                }
                bytes = v.bytes();
                // A failed attempt's files stay for a retry; the record
                // holds what is really on disk, not what was planned.
                v.estimated_bytes = figure(v, bytes);
                v.state = KeptState::Failed {
                    reason: reason.clone(),
                };
            });
            if applied.unwrap_or(true) {
                emit(
                    true,
                    format!(
                        "pharos: video keep failed item={item} reason={reason} files={files} bytes={bytes} ms={ms}"
                    ),
                );
            } else {
                superseded("superseded");
            }
        }
    }
    // Whatever this run left half staged (a file in flight when the
    // download failed or was dropped) goes with it.
    let cleaned = drop_parts_of(&inner.item_dir(&user, &item), Some(run));
    let owned = {
        let mut st = lock(&inner.state);
        if !cleaned || !disk.exact {
            let dir = inner.item_dir(&user, &item);
            if !st.leftover.contains(&dir) {
                st.leftover.push(dir);
            }
        }
        let owned = st.runs.get(&(user.clone(), item.clone())) == Some(&run);
        if owned {
            st.runs.remove(&(user, item));
        }
        owned
    };
    // A revoked run's writer is done: a folder its removal could not
    // delete or bound while it ran is deleted or measured now.
    if !owned || !cleaned || !disk.exact {
        inner.sweep_tombstones();
    }
}

/// Deletes the files a refused new download had stored, and forgets them,
/// while `run` still owns the item. Each check and deletion is one step
/// under the state lock, so a removal and restart in between cannot have
/// its replacement's file at the same path deleted from under it.
fn drop_refused_files(inner: &Inner, user: &str, item: &str, run: u64, keys: &[String]) {
    for key in keys {
        let mut st = lock(&inner.state);
        if st.runs.get(&(user.to_owned(), item.to_owned())) != Some(&run) {
            return;
        }
        let _ = std::fs::remove_file(inner.file_path(user, item, key));
        st.index.remove(&(user.to_owned(), key.clone()));
    }
}

/// Deletes one staging file; a test can make it fail.
fn remove_part(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::FAIL_PART_DELETE.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("part in use"));
    }
    std::fs::remove_file(path)
}

/// The staging files ending in `suffix` in `dir/files`, and whether the
/// listing was complete: false when the folder or an entry could not be
/// read (anything but the folder being gone).
fn part_paths(dir: &Path, suffix: &str) -> (Vec<PathBuf>, bool) {
    let read = match std::fs::read_dir(dir.join("files")) {
        Ok(read) => read,
        Err(e) => return (Vec::new(), e.kind() == std::io::ErrorKind::NotFound),
    };
    let mut clean = true;
    let mut paths = Vec::new();
    for entry in read {
        #[cfg(test)]
        let entry = tests::entry_read(entry);
        match entry {
            Ok(entry) => {
                if entry.file_name().to_string_lossy().ends_with(suffix) {
                    paths.push(entry.path());
                }
            }
            // An entry that cannot be read may be a staging file.
            Err(_) => clean = false,
        }
    }
    (paths, clean)
}

/// Deletes the staging files `run` left in `dir/files` (every run's, with
/// `None`); false when one would not go, or the folder or an entry in it
/// could not be read.
fn drop_parts_of(dir: &Path, run: Option<u64>) -> bool {
    let suffix = run.map_or_else(|| ".part".to_owned(), |r| format!(".{r}.part"));
    let (paths, mut clean) = part_paths(dir, &suffix);
    for path in paths {
        match remove_part(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => clean = false,
        }
    }
    clean
}

impl Inner {
    fn spawn_run(self: &Arc<Self>, user: String, item: String) {
        let run_id = {
            let mut st = lock(&self.state);
            st.next_run = st.next_run.wrapping_add(1);
            let id = st.next_run;
            st.runs.insert((user.clone(), item.clone()), id);
            id
        };
        let handle = self
            .env
            .runtime
            .spawn(run(self.clone(), user, item, run_id));
        #[cfg(test)]
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(handle);
        #[cfg(not(test))]
        drop(handle);
    }
}

impl DiskVideoStore {
    /// Opens the store at `root`: reads every manifest, indexes the whole
    /// files, and restarts the downloads a closed app left half done.
    pub fn open(root: PathBuf, env: Env) -> Arc<Self> {
        let inner = Arc::new(Inner {
            root,
            env,
            state: Mutex::new(State::default()),
            saves: Mutex::new(()),
            #[cfg(test)]
            tasks: Mutex::new(Vec::new()),
        });
        let mut list = Vec::new();
        let mut index = HashMap::new();
        let mut tombstones: Vec<(PathBuf, u64)> = Vec::new();
        let mut held: Vec<(PathBuf, u64)> = Vec::new();
        let mut unmeasured: Vec<(PathBuf, String)> = Vec::new();
        let mut measured: HashMap<PathBuf, (u64, bool)> = HashMap::new();
        let mut leftover: Vec<PathBuf> = Vec::new();
        for user_dir in list_dirs(&inner.root, &mut unmeasured) {
            for item_dir in list_dirs(&user_dir, &mut unmeasured) {
                let manifest = item_dir.join("manifest.json");
                let record = match std::fs::read(&manifest) {
                    Ok(bytes) => match serde_json::from_slice::<KeptVideo>(&bytes) {
                        Ok(v) => v,
                        Err(e) => {
                            hold_folder(
                                &item_dir,
                                &format!("record damaged: {e}"),
                                &mut held,
                                &mut unmeasured,
                            );
                            continue;
                        }
                    },
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        // A record that is there but cannot be read (a
                        // permissions fault, say) is not a removed one:
                        // its folder is left alone, not deleted.
                        hold_folder(
                            &item_dir,
                            &format!("record unreadable: {e}"),
                            &mut held,
                            &mut unmeasured,
                        );
                        continue;
                    }
                    Err(_) => {
                        // No record: a removed download whose folder did
                        // not delete. Try again; what stays still counts.
                        if let Err(e) = (inner.env.remove_dir)(&item_dir) {
                            // The figure from before the restart is gone:
                            // what is measured now is all that counts, and
                            // a folder not seen whole blocks admission.
                            let m = dir_size(&item_dir);
                            emit(
                                true,
                                format!(
                                    "pharos: video keep tombstone dir={} bytes={} exact={} error={e}",
                                    item_dir.display(),
                                    m.bytes,
                                    m.exact
                                ),
                            );
                            measured.insert(item_dir.clone(), (0, m.exact));
                            tombstones.push((item_dir.clone(), m.bytes));
                        }
                        continue;
                    }
                };
                // A staging file that will not go keeps the folder unknown
                // until a sweep deletes it or measures the folder whole.
                if !drop_parts_of(&item_dir, None) {
                    leftover.push(item_dir.clone());
                }
                let names = stored_names(&item_dir);
                let mut whole = 0usize;
                for key in &record.files {
                    if let Some(len) = names.get(&file_name(key)) {
                        whole += 1;
                        index.insert(
                            (record.user.clone(), key.clone()),
                            (item_dir.join("files").join(file_name(key)), *len),
                        );
                    }
                }
                let mut record = record;
                if record.ready() && whole < record.files.len() {
                    emit(
                        true,
                        format!(
                            "pharos: video record incomplete item={} files={} stored={whole}",
                            record.item,
                            record.files.len()
                        ),
                    );
                    let total = u32::try_from(record.files.len()).unwrap_or(u32::MAX);
                    record.state = KeptState::Downloading {
                        done: u32::try_from(whole).unwrap_or(u32::MAX),
                        total,
                        bytes: 0,
                    };
                }
                list.push(record);
            }
        }
        list.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));
        let restart = interrupted(&list);
        {
            let mut st = lock(&inner.state);
            st.list = list;
            st.index = index;
            st.tombstones = tombstones;
            st.held = held;
            st.unmeasured = unmeasured;
            st.measured = measured;
            st.leftover = leftover;
            st.root.clone_from(&inner.root);
        }
        emit(
            false,
            format!(
                "pharos: video store open root={} videos={} resume={}",
                inner.root.display(),
                lock(&inner.state).list.len(),
                restart.len()
            ),
        );
        for v in restart {
            emit(
                false,
                format!(
                    "pharos: video keep resume item={} files={}",
                    v.item,
                    v.files.len()
                ),
            );
            inner.spawn_run(v.user, v.item);
        }
        Arc::new(Self { inner })
    }

    /// Bytes in staging files not yet whole, all downloads (tests).
    #[cfg(test)]
    fn staged_bytes(&self) -> u64 {
        lock(&self.inner.state).staged.values().sum()
    }

    /// Waits for every download started so far to stop (tests).
    #[cfg(test)]
    async fn settled(&self) {
        loop {
            let next = self
                .inner
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            match next {
                Some(task) => {
                    let _ = task.await;
                }
                None => break,
            }
        }
    }
}

/// [`dirs`] for `open`: a folder that cannot be listed hides downloads that
/// may be there, so its size is unknown and nothing is admitted while it
/// stays.
fn list_dirs(dir: &Path, unmeasured: &mut Vec<(PathBuf, String)>) -> Vec<PathBuf> {
    dirs(dir).unwrap_or_else(|e| {
        emit(
            true,
            format!(
                "pharos: video folder unlistable dir={} error={e} admission=blocked",
                dir.display()
            ),
        );
        unmeasured.push((dir.to_path_buf(), format!("cannot be listed: {e}")));
        Vec::new()
    })
}

/// The folders directly under `dir`. A folder that is not there lists
/// nothing; one that cannot be listed (or whose entries cannot all be
/// read) is an error, not an empty list: what is under it is unknown.
fn dirs(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    #[cfg(test)]
    if tests::UNREADABLE.with(|u| u.borrow().iter().any(|p| p == dir)) {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in read {
        let path = entry?.path();
        // `is_dir` would read a failed lookup as "not a folder" and drop a
        // folder that may hold downloads. Symlinks are followed, as
        // `is_dir` did; one that dangles (or an entry that vanished) is
        // nothing to list.
        #[cfg(test)]
        if tests::UNLOOKUPABLE.with(|u| u.borrow().contains(&path)) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => out.push(path),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

impl VideoStore for DiskVideoStore {
    fn videos(&self, user: &str) -> Vec<KeptVideo> {
        lock(&self.inner.state)
            .list
            .iter()
            .filter(|v| v.user == user)
            .cloned()
            .collect()
    }

    fn start(&self, request: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
        let (user, item) = (request.user.clone(), request.item.clone());
        let budget = &self.inner.env.budget;
        // The size is not known until the playlists are read, so this
        // catches only a full cap or a full disk; the rest is checked after
        // planning and shows as a failed download. The room is read under
        // the host's shared admission lock, before the store's own lock is
        // taken, and the check and the new record sit under one hold of it.
        self.inner.sweep_tombstones();
        let mut replaced: Option<KeptVideo> = None;
        // The figure of a tombstone this start absorbed, to put back if the
        // start is rolled back.
        let mut absorbed: Option<u64> = None;
        let (started, used) = {
            let _admission = budget.admission_guard();
            // Readiness first, then the figures (see `Check::Fresh`).
            let ready = budget.accounting_ready();
            let (other, cap) = (budget.other_used_bytes(), budget.cap_bytes());
            let mut st = lock(&self.inner.state);
            let used = st.reserved_by_others(&user, &item).saturating_add(other);
            // Owed bytes first, then the free space (as in `admit`).
            let owed = st
                .outstanding(&user, &item)
                .saturating_add(budget.other_outstanding_bytes());
            let room = budget.free_bytes().saturating_sub(owed);
            let dir = self.inner.item_dir(&user, &item);
            let started = st
                .unknown_folder()
                .map_or(Ok(()), Err)
                .and_then(|()| {
                    if ready {
                        Ok(())
                    } else {
                        Err(KeepVideoRefusal::Host(
                            "the music cache is not up yet, so the shared cap cannot be checked"
                                .to_owned(),
                        ))
                    }
                })
                .and_then(|()| {
                    // The folder is being swept: a download begun in it now
                    // would have its files deleted.
                    if st.deleting.contains(&dir) || st.settling.contains(&dir) {
                        Err(KeepVideoRefusal::Host(format!(
                            "{} is being cleaned up, try again in a moment",
                            dir.display()
                        )))
                    } else {
                        Ok(())
                    }
                })
                .and_then(|()| fits(0, used, cap, room))
                .and_then(|()| {
                    check_not_kept(&st.list, &user, &item)?;
                    replaced = find(&st.list, &user, &item).cloned();
                    let mut record = new_record(&request, (self.inner.env.now)());
                    // A failed attempt's files stay on disk until a plan prunes
                    // them; the replacement keeps counting them meanwhile.
                    if let Some(old) = find(&st.list, &user, &item) {
                        record.estimated_bytes = old.estimated_bytes;
                    }
                    // So do the bytes of a removed download's folder that would
                    // not delete: the record takes the folder over whole. Its
                    // reservation floor becomes the tombstone's figure (a
                    // valid measurement, or the start was refused above as
                    // unknown), and the tombstone goes in the same hold, so
                    // the folder is accounted by the record as any running
                    // download's is, and a later writer's epoch bump cannot
                    // mark a folder the record owns as unknown.
                    if let Some((_, left)) = st.tombstones.iter().find(|(d, _)| *d == dir) {
                        let left = *left;
                        record.estimated_bytes = record.estimated_bytes.max(left);
                        absorbed = Some(left);
                        st.drop_tombstone(&dir);
                    }
                    insert_front(&mut st.list, record);
                    // A settlement that began before this start must not act
                    // on the folder as it saw it.
                    st.bump(&dir);
                    Ok(())
                });
            (started, used)
        };
        if let Err(refusal) = started {
            emit(
                true,
                format!("pharos: video keep refused item={item} reason={refusal} used={used}"),
            );
            return Err(refusal);
        }
        // A record that cannot be written would not survive a restart: the
        // download is refused, and the accounting goes back to what it was
        // (the record it replaced is put back, its reservation with it).
        if let Err(e) = self.inner.save_record(&user, &item) {
            let refusal =
                KeepVideoRefusal::Host(format!("the download's record could not be saved: {e}"));
            {
                let _admission = budget.admission_guard();
                let mut st = lock(&self.inner.state);
                take(&mut st.list, &user, &item);
                if let Some(old) = replaced {
                    insert_front(&mut st.list, old);
                }
                if let Some(left) = absorbed {
                    // The folder is the tombstone's again, as it was measured.
                    let dir = self.inner.item_dir(&user, &item);
                    let epoch = st.epoch(&dir);
                    st.tombstones.push((dir.clone(), left));
                    st.measured.insert(dir, (epoch, true));
                }
            }
            emit(
                true,
                format!(
                    "pharos: video keep refused item={item} reason={refusal} verdict=rolled_back"
                ),
            );
            return Err(refusal);
        }
        self.inner.spawn_run(user, item);
        Ok(())
    }

    fn remove(&self, user: &str, item: &str) {
        // Held to the end: no record is written into the folder while it
        // goes, and none after.
        let _saving = self
            .inner
            .saves
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let record = {
            let mut st = lock(&self.inner.state);
            let Some(record) = take(&mut st.list, user, item) else {
                return;
            };
            st.runs.remove(&(user.to_owned(), item.to_owned()));
            for key in &record.files {
                st.index.remove(&(user.to_owned(), key.clone()));
            }
            // What the record reserved keeps counting, as a tombstone, from
            // the moment the record leaves the list: there is no instant
            // when the folder counts for nothing while it is deleted.
            let dir = self.inner.item_dir(user, item);
            st.tombstones.retain(|(d, _)| *d != dir);
            st.tombstones.push((dir.clone(), reserved(&record)));
            // The reservation is an estimate, not a bound on the folder
            // (staged `.part` files, files an earlier plan left, a file
            // larger than planned), so until the folder is deleted or
            // measured after the last thing that can change it, its size
            // is not known (`State::unknown_reason`): nothing is admitted.
            st.removing.push(dir.clone());
            st.measured.remove(&dir);
            st.bump(&dir);
            record
        };
        let dir = self.inner.item_dir(user, item);
        // The record goes first, so a folder that will not delete cannot
        // bring the download back at the next open.
        match std::fs::remove_file(dir.join("manifest.json")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                // The record is still on disk and would bring the download
                // back at the next open, so the removal has not happened:
                // the record stays, as a failed one, and the caller sees it.
                self.inner.keep_after_failed_unlink(record, &dir, &e);
                return;
            }
        }
        let removed = (self.inner.env.remove_dir)(&dir);
        let mut left = 0;
        if removed.is_ok() {
            lock(&self.inner.state).drop_tombstone(&dir);
        } else {
            // The bytes still on disk keep counting against the cap until
            // a later sweep deletes the folder. The removal is over: the
            // folder is no longer in removal, and is measured now unless a
            // revoked run's writer is still in it, in which case that
            // run's end measures (it sweeps after its writer stopped). The
            // folder is unknown until then, by derivation.
            lock(&self.inner.state).removing.retain(|d| *d != dir);
            #[cfg(test)]
            tests::after_removal_unmarked();
            left = self.inner.measure_tombstone(&dir);
        }
        emit(
            false,
            format!(
                "pharos: video keep remove item={item} files={} verdict={}",
                record.files.len(),
                match &removed {
                    Ok(()) => "removed".to_owned(),
                    Err(e) => format!("kept_on_disk bytes={left} error={e}"),
                }
            ),
        );
    }

    fn save_position(&self, user: &str, item: &str, position_ms: u64) {
        // The position stays in memory if the save fails, and the failure
        // is logged by `save_record` (the trait has no way to return it).
        let _ = self
            .inner
            .update(user, item, |v| v.position_ms = position_ms);
    }

    fn cap_bytes(&self) -> u64 {
        self.inner.env.budget.cap_bytes()
    }

    fn size_unknown(&self) -> bool {
        lock(&self.inner.state).unknown_folder().is_some()
    }

    fn outstanding_bytes(&self) -> u64 {
        // No record has an empty user, so this is every record's.
        lock(&self.inner.state).outstanding("", "")
    }

    fn kept_bytes(&self) -> u64 {
        let st = lock(&self.inner.state);
        st.list
            .iter()
            .map(reserved)
            .fold(0u64, u64::saturating_add)
            .saturating_add(st.tombstoned())
    }

    fn used_bytes(&self, user: &str) -> u64 {
        let video = lock(&self.inner.state)
            .list
            .iter()
            .filter(|v| v.user == user)
            .map(KeptVideo::bytes)
            .sum::<u64>();
        video.saturating_add(self.inner.env.budget.other_used_bytes())
    }

    fn free_bytes(&self) -> u64 {
        self.inner.env.budget.free_bytes()
    }

    fn stored_file(&self, user: &str, key: &str) -> Option<StoredFile> {
        let (path, len) = lock(&self.inner.state)
            .index
            .get(&(user.to_owned(), key.to_owned()))
            .cloned()?;
        Some(StoredFile {
            path,
            len,
            content_type: content_type_for(key),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use super::*;
    use crate::native::kept_video::DEFAULT_QUALITY_BPS;
    use crate::native::offline_ui::{FREE_GUARD_BYTES, GIB};

    thread_local! {
        /// While set, deleting a folder fails (a file in use, say).
        static FAIL_REMOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// With `FAIL_REMOVE`, the folder is also left unreadable: a plain
        /// file stands where it was.
        static LEAVE_UNREADABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    thread_local! {
        /// Folders `dir_size` cannot read (a root user cannot be denied).
        pub(super) static UNREADABLE: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    }

    thread_local! {
        /// While set, a chunk's write is held pending before its flush.
        static HOLD_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    thread_local! {
        /// While set, the chunk writes of this item alone are held.
        static HOLD_ITEM: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(super) async fn hold_write(item: &str) {
        while HOLD_WRITE.with(std::cell::Cell::get)
            || HOLD_ITEM.with(|h| h.borrow().as_deref() == Some(item))
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    thread_local! {
        /// Entries whose metadata lookup fails after they were listed.
        pub(super) static UNLOOKUPABLE: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    }

    thread_local! {
        /// Runs as a folder is being deleted.
        static DURING_REMOVE: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
    }

    type SaveHook = Box<dyn Fn(&KeptVideo)>;

    thread_local! {
        /// Runs as a tombstoned folder has been walked, before the result is
        /// taken.
        static DURING_MEASURE: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
        /// Runs once a removal has left its folder, before it is measured.
        static AFTER_UNMARKED: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
    }

    pub(super) fn during_measure() {
        DURING_MEASURE.with(|d| {
            if let Some(f) = d.borrow().as_ref() {
                f();
            }
        });
    }

    thread_local! {
        /// Runs once a settlement has claimed its folder, before it drops a
        /// staging file.
        static BEFORE_SETTLE_DROP: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
        /// While set, reading a directory entry fails after the directory
        /// opened.
        pub(super) static FAIL_ENTRY_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn before_settle_drop() {
        BEFORE_SETTLE_DROP.with(|d| {
            if let Some(f) = d.borrow().as_ref() {
                f();
            }
        });
    }

    /// An entry as read from a listing; a test can make the read fail.
    pub(super) fn entry_read(
        entry: std::io::Result<std::fs::DirEntry>,
    ) -> std::io::Result<std::fs::DirEntry> {
        if FAIL_ENTRY_READ.with(std::cell::Cell::get) {
            return Err(std::io::Error::other("entry unreadable"));
        }
        entry
    }

    pub(super) fn after_removal_unmarked() {
        AFTER_UNMARKED.with(|d| {
            if let Some(f) = d.borrow().as_ref() {
                f();
            }
        });
    }

    thread_local! {
        /// While set, the walk of a folder's whole files comes back short
        /// (as a transient EMFILE would leave it).
        pub(super) static FAIL_DISK_WALK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    thread_local! {
        /// While set, deleting a staging file fails.
        pub(super) static FAIL_PART_DELETE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    thread_local! {
        /// Runs as a record is about to be written, with that record.
        static DURING_SAVE: RefCell<Option<SaveHook>> = RefCell::new(None);
    }

    pub(super) fn during_save(video: &KeptVideo) {
        DURING_SAVE.with(|d| {
            if let Some(f) = d.borrow().as_ref() {
                f(video);
            }
        });
    }

    thread_local! {
        /// Runs when a removal's manifest unlink has failed, before the
        /// record is put back.
        static AT_UNLINK_FAILURE: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
    }

    pub(super) fn at_unlink_failure() {
        AT_UNLINK_FAILURE.with(|d| {
            if let Some(f) = d.borrow().as_ref() {
                f();
            }
        });
    }

    fn removing(dir: &Path) -> std::io::Result<()> {
        DURING_REMOVE.with(|d| {
            if let Some(f) = d.borrow().as_ref() {
                f();
            }
        });
        if FAIL_REMOVE.with(std::cell::Cell::get) {
            if LEAVE_UNREADABLE.with(std::cell::Cell::get) {
                let _ = std::fs::remove_dir_all(dir);
                let _ = std::fs::write(dir, b"x");
            }
            Err(std::io::Error::other("in use"))
        } else {
            std::fs::remove_dir_all(dir)
        }
    }

    thread_local! {
        pub(super) static CAPTURED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn captured() -> Vec<String> {
        CAPTURED.with(|c| c.borrow().clone())
    }

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.64001f\"\n\
        /videos/1/h264cmaf/main.m3u8?PlaySessionId=p\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n\
        #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"init.mp4?PlaySessionId=p\"\n\
        #EXTINF:6.0,\n0.m4s?PlaySessionId=p\n#EXTINF:6.0,\n1.m4s?PlaySessionId=p\n#EXT-X-ENDLIST\n";
    const MASTER_KEY: &str = "/videos/1/master.m3u8?PlaySessionId=p";
    const MEDIA_KEY: &str = "/videos/1/h264cmaf/main.m3u8?PlaySessionId=p";
    const SEG0: &str = "/videos/1/h264cmaf/0.m4s?PlaySessionId=p";
    const SEG1: &str = "/videos/1/h264cmaf/1.m4s?PlaySessionId=p";
    const INIT: &str = "/videos/1/h264cmaf/init.mp4?PlaySessionId=p";

    /// A server in memory: a key answers its bytes, or fails as scripted.
    #[derive(Default)]
    struct FakeServer {
        files: Mutex<HashMap<String, Vec<u8>>>,
        /// key -> statuses to answer before the file (`0` = no network).
        failures: Mutex<HashMap<String, Vec<u16>>>,
        asked: Mutex<Vec<String>>,
        /// key -> a flag; the first request for the key waits for the flag
        /// to be set, then answers 403.
        holds: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
        /// key -> a flag; the first body for the key stops after its first
        /// chunk until the flag is set.
        mid_holds: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
        /// key -> a flag; the first request for the key waits for the flag,
        /// then is answered as any other.
        slow_opens: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    }

    impl FakeServer {
        /// Holds the first request for `key` until the returned flag is set.
        fn hold_first(&self, key: &str) -> Arc<std::sync::atomic::AtomicBool> {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.holds.lock().unwrap().insert(key.into(), flag.clone());
            flag
        }

        /// Holds the first request for `key` until the flag is set, then
        /// answers it normally.
        fn slow_first_open(&self, key: &str) -> Arc<std::sync::atomic::AtomicBool> {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.slow_opens
                .lock()
                .unwrap()
                .insert(key.into(), flag.clone());
            flag
        }

        /// Holds the first body for `key` after its first chunk.
        fn hold_mid_body(&self, key: &str) -> Arc<std::sync::atomic::AtomicBool> {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.mid_holds
                .lock()
                .unwrap()
                .insert(key.into(), flag.clone());
            flag
        }

        /// Waits until `key` has been asked for `times` times.
        async fn asked_times(&self, key: &str, times: usize) {
            while self.asked().iter().filter(|k| *k == key).count() < times {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }

        fn with_stream() -> Arc<Self> {
            let s = Arc::new(Self::default());
            let mut f = s.files.lock().unwrap();
            f.insert(MASTER_KEY.into(), MASTER.as_bytes().to_vec());
            f.insert(MEDIA_KEY.into(), MEDIA.as_bytes().to_vec());
            f.insert(INIT.into(), vec![1; 10]);
            f.insert(SEG0.into(), vec![2; 100]);
            f.insert(SEG1.into(), vec![3; 100]);
            drop(f);
            s
        }

        fn fail(&self, key: &str, statuses: &[u16]) {
            self.failures
                .lock()
                .unwrap()
                .insert(key.into(), statuses.to_vec());
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    struct FakeBody(Vec<Vec<u8>>, Option<Arc<std::sync::atomic::AtomicBool>>);

    impl Body for FakeBody {
        fn next_chunk(&mut self) -> BoxFut<'_, Result<Option<Vec<u8>>, FetchError>> {
            Box::pin(async move {
                // A held body waits before its second chunk.
                if let (Some(flag), 1) = (self.1.clone(), self.0.len()) {
                    while !flag.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                }
                Ok(if self.0.is_empty() {
                    None
                } else {
                    Some(self.0.remove(0))
                })
            })
        }
    }

    impl Fetcher for FakeServer {
        fn open<'a>(&'a self, key: &'a str) -> BoxFut<'a, Result<Box<dyn Body>, FetchError>> {
            Box::pin(async move {
                self.asked.lock().unwrap().push(key.to_owned());
                let slow = self.slow_opens.lock().unwrap().remove(key);
                if let Some(flag) = slow {
                    while !flag.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                }
                let hold = self.holds.lock().unwrap().remove(key);
                if let Some(flag) = hold {
                    while !flag.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    return Err(FetchError {
                        status: Some(403),
                        message: String::new(),
                    });
                }
                if let Some(next) = {
                    let mut f = self.failures.lock().unwrap();
                    f.get_mut(key)
                        .filter(|v| !v.is_empty())
                        .map(|v| v.remove(0))
                } {
                    return Err(FetchError {
                        status: (next != 0).then_some(next),
                        message: "no signal".into(),
                    });
                }
                match self.files.lock().unwrap().get(key) {
                    // Two chunks, so a body really streams.
                    Some(b) => {
                        let mid = b.len() / 2;
                        let gate = self.mid_holds.lock().unwrap().remove(key);
                        Ok(Box::new(FakeBody(
                            vec![
                                b.get(..mid).unwrap_or_default().to_vec(),
                                b.get(mid..).unwrap_or_default().to_vec(),
                            ],
                            gate,
                        )) as Box<dyn Body>)
                    }
                    None => Err(FetchError {
                        status: Some(403),
                        message: String::new(),
                    }),
                }
            })
        }
    }

    struct FakeBudget {
        cap: AtomicU64,
        other: AtomicU64,
        other_outstanding: AtomicU64,
        ready: AtomicBool,
        free: AtomicU64,
        /// Admission guards held now, taken in all, and readings made with
        /// none held.
        held: Arc<AtomicUsize>,
        taken: AtomicUsize,
        unguarded: AtomicUsize,
        /// Runs whenever the free space is measured.
        on_free: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    struct FakeGuard(Arc<AtomicUsize>);

    impl Drop for FakeGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl FakeBudget {
        fn read(&self) {
            if self.held.load(Ordering::SeqCst) == 0 {
                self.unguarded.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl FakeBudget {
        fn roomy() -> Arc<Self> {
            Arc::new(Self {
                cap: AtomicU64::new(4 * GIB),
                other: AtomicU64::new(0),
                other_outstanding: AtomicU64::new(0),
                ready: AtomicBool::new(true),
                free: AtomicU64::new(100 * GIB),
                held: Arc::default(),
                taken: AtomicUsize::new(0),
                unguarded: AtomicUsize::new(0),
                on_free: Mutex::new(None),
            })
        }
    }

    impl Budget for FakeBudget {
        fn cap_bytes(&self) -> u64 {
            self.read();
            self.cap.load(Ordering::SeqCst)
        }
        fn other_used_bytes(&self) -> u64 {
            self.read();
            self.other.load(Ordering::SeqCst)
        }
        fn other_outstanding_bytes(&self) -> u64 {
            self.read();
            self.other_outstanding.load(Ordering::SeqCst)
        }
        fn accounting_ready(&self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
        fn free_bytes(&self) -> u64 {
            self.read();
            if let Some(hook) = self.on_free.lock().unwrap().as_ref() {
                hook();
            }
            self.free.load(Ordering::SeqCst)
        }
        fn admission_guard(&self) -> Box<dyn std::any::Any> {
            self.held.fetch_add(1, Ordering::SeqCst);
            self.taken.fetch_add(1, Ordering::SeqCst);
            Box::new(FakeGuard(self.held.clone()))
        }
    }

    struct Rig {
        dir: PathBuf,
        server: Arc<FakeServer>,
        budget: Arc<FakeBudget>,
        slept: Arc<Mutex<Vec<u64>>>,
        opened: AtomicUsize,
    }

    impl Rig {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pharos-kept-video-{tag}-{}-{}",
                std::process::id(),
                iso_now().replace(':', "")
            ));
            let _ = std::fs::remove_dir_all(&dir);
            CAPTURED.with(|c| c.borrow_mut().clear());
            Self {
                dir,
                server: FakeServer::with_stream(),
                budget: FakeBudget::roomy(),
                slept: Arc::default(),
                opened: AtomicUsize::new(0),
            }
        }

        fn open(&self) -> Arc<DiskVideoStore> {
            self.open_with(self.budget.clone())
        }

        fn open_with(&self, budget: Arc<dyn Budget>) -> Arc<DiskVideoStore> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            let slept = self.slept.clone();
            DiskVideoStore::open(
                self.dir.clone(),
                Env {
                    fetcher: self.server.clone(),
                    budget,
                    sleep: Arc::new(move |secs| {
                        slept.lock().unwrap().push(secs);
                        Box::pin(async {})
                    }),
                    now: || "2026-11-16T09:00:00Z".to_owned(),
                    runtime: tokio::runtime::Handle::current(),
                    remove_dir: removing,
                },
            )
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn request(item: &str) -> KeepVideoRequest {
        KeepVideoRequest {
            item: item.into(),
            version: String::new(),
            user: "u1".into(),
            title: "Film one".into(),
            kind: "movie".into(),
            runtime_ms: 12_000,
            master_url: MASTER_KEY.into(),
            audio: None,
            subtitles: vec![],
            quality_bps: DEFAULT_QUALITY_BPS,
        }
    }

    fn only(store: &DiskVideoStore) -> KeptVideo {
        let v = store.videos("u1");
        assert_eq!(v.len(), 1, "{v:?}");
        v[0].clone()
    }

    fn lines(prefix: &str) -> Vec<String> {
        captured()
            .into_iter()
            .filter(|l| l.starts_with(prefix))
            .collect()
    }

    #[tokio::test]
    async fn a_download_fetches_every_file_and_ends_ready() {
        let rig = Rig::new("whole");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let v = only(&store);
        assert_eq!(v.state, KeptState::Ready, "{v:?}");
        assert_eq!(v.files, [MASTER_KEY, MEDIA_KEY, INIT, SEG0, SEG1]);
        assert_eq!(
            v.estimated_bytes,
            MASTER.len() as u64 + MEDIA.len() as u64 + 210
        );
        for (key, byte) in [(SEG0, 2u8), (INIT, 1), (SEG1, 3)] {
            let f = store.stored_file("u1", key).unwrap();
            assert_eq!(std::fs::read(&f.path).unwrap().first(), Some(&byte));
            assert_eq!(f.len, std::fs::metadata(&f.path).unwrap().len());
        }
        assert_eq!(
            store.stored_file("u1", SEG0).unwrap().content_type,
            "video/mp4"
        );
        assert_eq!(
            store.stored_file("u1", MEDIA_KEY).unwrap().content_type,
            "application/vnd.apple.mpegurl"
        );
        assert!(store.stored_file("u1", "/videos/1/other.m4s").is_none());
        let dir = rig.dir.join("u1").join("1").join("files");
        assert!(
            std::fs::read_dir(dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")),
            "no temp file is left"
        );
    }

    #[tokio::test]
    async fn the_signals_name_start_and_end_with_the_item_and_its_size() {
        let rig = Rig::new("signals");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let start = lines("pharos: video keep start");
        assert_eq!(start.len(), 1);
        assert!(
            start[0].contains("item=1") && start[0].contains("files=5"),
            "{start:?}"
        );
        let ready = lines("pharos: video keep ready");
        assert_eq!(ready.len(), 1);
        assert!(
            ready[0].contains("item=1")
                && ready[0].contains("files=5")
                && ready[0].contains("bytes="),
            "{ready:?}"
        );
        assert!(lines("pharos: video keep failed").is_empty());
    }

    #[tokio::test]
    async fn a_restart_fetches_only_the_files_that_are_missing() {
        let rig = Rig::new("resume");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let gone = store.stored_file("u1", SEG1).unwrap().path;
        drop(store);
        std::fs::remove_file(gone).unwrap();
        rig.server.asked.lock().unwrap().clear();

        let store = rig.open();
        assert_eq!(
            lines("pharos: video keep resume").len(),
            1,
            "a ready copy with a file gone is resumed"
        );
        store.settled().await;
        assert_eq!(
            rig.server.asked(),
            [SEG1],
            "only the missing file is asked for"
        );
        assert_eq!(only(&store).state, KeptState::Ready);
        assert!(store.stored_file("u1", SEG1).is_some());
    }

    #[tokio::test]
    async fn an_interrupted_download_resumes_after_a_restart() {
        let rig = Rig::new("interrupted");
        rig.server.fail(SEG1, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let v = only(&store);
        assert!(matches!(v.state, KeptState::Failed { .. }), "{v:?}");
        drop(store);

        // The app was closed with it half done: the record says Downloading.
        let manifest = rig.dir.join("u1").join("1").join("manifest.json");
        let mut v: KeptVideo = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        v.state = KeptState::Downloading {
            done: 4,
            total: 5,
            bytes: 1,
        };
        std::fs::write(&manifest, serde_json::to_vec(&v).unwrap()).unwrap();
        rig.server.asked.lock().unwrap().clear();
        let store = rig.open();
        store.settled().await;
        // The failure may have cut a file in flight beside it short (the
        // init or the other segment), so those may be asked for again; the
        // playlists, whole before any media file starts, are not.
        let asked = rig.server.asked();
        assert!(asked.contains(&SEG1.to_owned()), "{asked:?}");
        assert!(
            asked.iter().all(|k| !k.contains(".m3u8")),
            "no playlist is refetched: {asked:?}"
        );
        assert_eq!(only(&store).state, KeptState::Ready);
    }

    #[tokio::test]
    async fn a_busy_server_is_retried_with_backoff_and_the_signal_names_the_status() {
        let rig = Rig::new("retry");
        rig.server.fail(SEG0, &[503, 0, 404]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
        assert_eq!(*rig.slept.lock().unwrap(), [1, 2, 4]);
        let failed = lines("pharos: video keep file failed");
        assert_eq!(failed.len(), 3);
        assert!(failed[0].contains("status=503") && failed[0].contains("attempt=1"));
        assert!(failed[1].contains("status=none") && failed[1].contains("attempt=2"));
        assert!(failed[2].contains("status=404"));
        assert!(failed[0].contains("url_path=/videos/1/h264cmaf/0.m4s"));
        assert!(
            !failed[0].contains("PlaySessionId"),
            "no query in a log line"
        );
    }

    #[tokio::test]
    async fn a_refused_file_fails_the_download_naming_the_path_and_status() {
        let rig = Rig::new("refused-file");
        rig.server.fail(SEG0, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("not failed");
        };
        assert!(
            reason.contains("/videos/1/h264cmaf/0.m4s") && reason.contains("403"),
            "{reason}"
        );
        assert!(rig.slept.lock().unwrap().is_empty(), "403 is not retried");
        let failed = lines("pharos: video keep failed");
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0].contains("item=1") && failed[0].contains("reason="),
            "{failed:?}"
        );
        // A failed download may be started again, and finishes.
        rig.server.failures.lock().unwrap().clear();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
    }

    #[tokio::test]
    async fn a_download_over_the_cap_is_refused_and_keeps_nothing() {
        let rig = Rig::new("over-cap");
        rig.budget.cap.store(1_000, Ordering::SeqCst);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("not failed");
        };
        assert!(reason.contains("cap"), "{reason}");
        let refused = lines("pharos: video keep refused");
        assert_eq!(refused.len(), 1);
        assert!(
            refused[0].contains("estimate=") && refused[0].contains("cap=1000"),
            "{refused:?}"
        );
        assert!(store.stored_file("u1", MASTER_KEY).is_none());
        assert!(
            !rig.server.asked().contains(&SEG0.to_owned()),
            "no segment was fetched"
        );
    }

    #[tokio::test]
    async fn the_musics_share_of_the_budget_counts_against_the_cap() {
        let rig = Rig::new("shared");
        // 4 GiB shared, music holding nearly all of it: the film does not fit.
        rig.budget.other.store(4 * GIB - 10, Ordering::SeqCst);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(matches!(only(&store).state, KeptState::Failed { .. }));
        assert_eq!(store.used_bytes("u1"), 4 * GIB - 10);
    }

    /// A music write the cache has approved, and not yet landed, is room a
    /// video admission cannot count: the free figure does not lack those
    /// bytes yet. (The phone's budget reports them from the cache's
    /// reservation, `Library::music_outstanding_bytes`.)
    #[tokio::test]
    async fn a_video_admission_does_not_count_room_an_approved_music_write_will_take() {
        let rig = Rig::new("music-approved");
        let free = 100 * GIB;
        rig.budget.free.store(free, Ordering::SeqCst);
        // Music is approved for all the room above the guard, and has yet to
        // write it.
        rig.budget
            .other_outstanding
            .store(free - FREE_GUARD_BYTES, Ordering::SeqCst);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("admitted against room the music write will take");
        };
        assert!(reason.contains("guard"), "{reason}");
        // Once the music write lands (or its job ends) the room is the video's.
        rig.budget.other_outstanding.store(0, Ordering::SeqCst);
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(only(&store).ready(), "still refused after the release");
    }

    /// The music side as the library reports it: a window held mid-download
    /// (`Library::music_outstanding_bytes`, covered in pharos-content by
    /// `a_download_held_mid_window_is_outstanding_until_the_job_ends`).
    struct FakeMusic {
        outstanding: AtomicU64,
    }

    impl crate::native::ports::MusicSide for FakeMusic {
        fn cache_cap(&self) -> Option<u64> {
            Some(100 * GIB)
        }
        fn used_bytes(&self) -> Option<u64> {
            Some(0)
        }
        fn cache_enabled(&self) -> bool {
            true
        }
        fn outstanding_bytes(&self) -> u64 {
            self.outstanding.load(Ordering::SeqCst)
        }
    }

    /// The phone's budget: every music figure through the shared functions
    /// `PhoneBudget` calls, the free space and the lock from the rig.
    struct MusicBudget {
        music: Arc<FakeMusic>,
        base: Arc<FakeBudget>,
    }

    impl Budget for MusicBudget {
        fn cap_bytes(&self) -> u64 {
            crate::native::ports::music_cap(Some(&*self.music))
        }
        fn other_used_bytes(&self) -> u64 {
            crate::native::ports::music_used(Some(&*self.music))
        }
        fn accounting_ready(&self) -> bool {
            crate::native::ports::music_ready(Some(&*self.music))
        }
        fn other_outstanding_bytes(&self) -> u64 {
            crate::native::ports::music_outstanding(Some(&*self.music))
        }
        fn free_bytes(&self) -> u64 {
            self.base.free_bytes()
        }
        fn admission_guard(&self) -> Box<dyn std::any::Any> {
            self.base.admission_guard()
        }
    }

    /// A music download held mid-window leaves one window of free space
    /// owed: a video that fits the free space only if that window is
    /// ignored is refused by the 2 GiB guard, and admitted once the
    /// music's reservation is released.
    #[tokio::test]
    async fn a_video_is_refused_by_the_guard_while_a_music_window_is_held() {
        let rig = Rig::new("music-window");
        // Room for the film above the guard, but not for it and the window.
        rig.budget
            .free
            .store(FREE_GUARD_BYTES + 4_000_000, Ordering::SeqCst);
        let music = Arc::new(FakeMusic {
            outstanding: AtomicU64::new(8 * 1024 * 1024),
        });
        let store = rig.open_with(Arc::new(MusicBudget {
            music: music.clone(),
            base: rig.budget.clone(),
        }));
        let refusal = store.start(request("1")).unwrap_err();
        assert!(
            refusal.to_string().contains("guard"),
            "refused, but not by the guard: {refusal}"
        );
        assert!(store.videos("u1").is_empty());
        music.outstanding.store(0, Ordering::SeqCst);
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(only(&store).ready(), "{:?}", only(&store).state);
    }

    #[tokio::test]
    async fn a_download_that_would_leave_under_the_guard_is_refused() {
        let rig = Rig::new("guard");
        rig.budget
            .free
            .store(FREE_GUARD_BYTES - 1, Ordering::SeqCst);
        let store = rig.open();
        // Under the guard already: refused before any request.
        let refusal = store.start(request("1")).unwrap_err();
        assert!(
            matches!(refusal, KeepVideoRefusal::LowSpace { .. }),
            "{refusal:?}"
        );
        assert!(store.videos("u1").is_empty());
        assert!(rig.server.asked().is_empty());
        assert_eq!(lines("pharos: video keep refused").len(), 1);

        // Room for the check but not for the film: refused after planning.
        rig.budget
            .free
            .store(FREE_GUARD_BYTES + 100, Ordering::SeqCst);
        store.start(request("1")).unwrap();
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("not failed");
        };
        assert!(reason.contains("guard"), "{reason}");
    }

    /// A file in flight when the download fails is dropped half staged;
    /// the run takes its staging file with it.
    /// A download resumed after a restart is held to the free-space guard
    /// for what is left to fetch, and 0 free (an unreadable measure) is no
    /// room.
    #[tokio::test]
    async fn a_resumed_download_needs_room_for_what_is_left() {
        for free in [0, FREE_GUARD_BYTES + 100] {
            let rig = Rig::new("resume-room");
            let store = rig.open();
            store.start(request("1")).unwrap();
            store.settled().await;
            let gone = store.stored_file("u1", SEG1).unwrap().path;
            drop(store);
            std::fs::remove_file(gone).unwrap();
            rig.server.asked.lock().unwrap().clear();
            rig.budget.free.store(free, Ordering::SeqCst);

            let store = rig.open();
            store.settled().await;
            let KeptState::Failed { reason } = only(&store).state else {
                panic!("free={free}: a resume without room went on");
            };
            assert!(reason.contains("guard"), "{reason}");
            assert!(rig.server.asked().is_empty(), "nothing was fetched");
            let refused = lines("pharos: video keep refused");
            assert!(
                refused.len() == 1
                    && refused[0].contains("resume=true")
                    && refused[0].contains(&format!("free={free}")),
                "{refused:?}"
            );
            // The stored files stay: with room again the copy finishes.
            assert!(store.stored_file("u1", SEG0).is_some());
        }
    }

    /// Two downloads that each fit alone but not together: the second
    /// is refused against the first's reservation, not admitted beside it.
    #[tokio::test]
    async fn a_second_download_is_held_to_the_firsts_reserved_estimate() {
        let rig = Rig::new("reserve");
        let estimate = estimate_bytes(2_000_000, 12_000);
        rig.budget.cap.store(estimate + 1_000, Ordering::SeqCst);
        // The first download stays under way, holding its reservation.
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        rig.server.asked_times(SEG0, 1).await;
        store.start(request("2")).unwrap();
        let mut waited = 0;
        loop {
            let second = store.videos("u1").into_iter().find(|v| v.item == "2");
            if second
                .as_ref()
                .is_some_and(|v| matches!(v.state, KeptState::Failed { .. }))
            {
                break;
            }
            waited += 1;
            assert!(
                waited < 2_000,
                "the second download was admitted: {second:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        release.store(true, Ordering::SeqCst);
        store.settled().await;
        let state = |item: &str| {
            store
                .videos("u1")
                .into_iter()
                .find(|v| v.item == item)
                .unwrap()
                .state
        };
        assert_eq!(state("1"), KeptState::Ready);
        let KeptState::Failed { reason } = state("2") else {
            panic!("the second was admitted");
        };
        assert!(reason.contains("cap"), "{reason}");
        let refused = lines("pharos: video keep refused");
        assert!(
            refused.len() == 1 && refused[0].contains("item=2"),
            "{refused:?}"
        );
    }

    /// Admission reads the other kind's bytes and reserves its own under the
    /// host's shared lock, so music cannot commit in between.
    #[tokio::test]
    async fn admission_reads_the_budget_only_under_the_shared_lock() {
        let rig = Rig::new("gate");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        // A restart resumes a copy with a file gone: another admission.
        let gone = store.stored_file("u1", SEG1).unwrap().path;
        drop(store);
        std::fs::remove_file(gone).unwrap();
        let store = rig.open();
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
        assert!(
            rig.budget.taken.load(Ordering::SeqCst) >= 3,
            "start, its plan, and the resumed plan each take the lock"
        );
        assert_eq!(
            rig.budget.unguarded.load(Ordering::SeqCst),
            0,
            "a reading was made without the shared lock"
        );
        assert_eq!(rig.budget.held.load(Ordering::SeqCst), 0, "all released");
    }

    /// The free space already lacks what a download has staged in a file not
    /// yet whole; only the rest of its estimate is still to come out of it.
    #[tokio::test]
    async fn staged_bytes_are_not_counted_as_still_to_write() {
        let rig = Rig::new("staged");
        let estimate = estimate_bytes(2_000_000, 12_000);
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        // The first download holds its SEG0 half written (50 bytes staged)
        // with everything else stored.
        let stored = (MASTER.len() + MEDIA.len() + 10 + 100) as u64;
        let mut waited = 0;
        loop {
            let bytes = match store.videos("u1").first().map(|v| v.state.clone()) {
                Some(KeptState::Downloading { bytes, .. }) => bytes,
                _ => 0,
            };
            if bytes == stored && store.staged_bytes() == 50 {
                break;
            }
            waited += 1;
            assert!(
                waited < 2_000,
                "bytes={bytes} staged={}",
                store.staged_bytes()
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        // The disk now lacks what was written: room for the second film
        // beyond the guard, exactly, once the first's staged bytes are not
        // counted twice.
        rig.budget.free.store(
            2 * estimate + FREE_GUARD_BYTES - stored - 50,
            Ordering::SeqCst,
        );
        store.start(request("2")).unwrap();
        let mut waited = 0;
        loop {
            let second = store.videos("u1").into_iter().find(|v| v.item == "2");
            if second.as_ref().is_some_and(|v| {
                v.estimated_bytes > 0 || matches!(v.state, KeptState::Failed { .. })
            }) {
                break;
            }
            waited += 1;
            assert!(waited < 2_000, "{second:?}");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        release.store(true, Ordering::SeqCst);
        store.settled().await;
        let videos = store.videos("u1");
        let second = videos.iter().find(|v| v.item == "2").unwrap();
        assert_eq!(
            second.state,
            KeptState::Ready,
            "refused for room it had: {second:?}"
        );
    }

    /// What the running downloads still owe is read before the free space
    /// is measured, in one critical section: a write landing in between
    /// would otherwise lower the owed figure after free was read and be
    /// counted as room. The measure is made under the store's state lock,
    /// after the owed figure is taken from it.
    #[tokio::test]
    async fn the_free_space_is_measured_after_what_is_owed_is_read() {
        let rig = Rig::new("owed-before-free");
        let store = rig.open();
        let inner = store.inner.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let locked = Arc::new(AtomicUsize::new(0));
        let (c, l) = (calls.clone(), locked.clone());
        *rig.budget.on_free.lock().unwrap() = Some(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            if inner.state.try_lock().is_err() {
                l.fetch_add(1, Ordering::SeqCst);
            }
        }));
        store.start(request("1")).unwrap();
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
        // Other readers measure free space too; the admission is the one
        // made under the lock.
        assert!(calls.load(Ordering::SeqCst) >= 1);
        assert!(
            locked.load(Ordering::SeqCst) >= 1,
            "free was read outside the owed read"
        );
    }

    /// An old run still draining after a restart stages bytes the new
    /// run's estimate does not cover: they do not reduce what is owed.
    #[tokio::test]
    async fn a_stale_runs_staged_bytes_do_not_reduce_what_is_owed() {
        let rig = Rig::new("stale-staged");
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        let mut waited = 0;
        while store.staged_bytes() != 50 {
            waited += 1;
            assert!(waited < 2_000, "staged={}", store.staged_bytes());
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let owed = store.outstanding_bytes();
        let run = lock(&store.inner.state).runs[&("u1".to_owned(), "1".to_owned())];
        let mut stale = Staged {
            inner: &store.inner,
            who: ("u1".to_owned(), "1".to_owned(), run + 1_000),
            len: 0,
            part: PathBuf::new(),
        };
        stale.add(7_000);
        assert_eq!(store.staged_bytes(), 7_050);
        assert_eq!(
            store.outstanding_bytes(),
            owed,
            "counted the old run's bytes"
        );
        drop(stale);
        assert_eq!(store.staged_bytes(), 50);
        release.store(true, Ordering::SeqCst);
        store.settled().await;
    }

    /// `start` reads what running downloads owe before it measures free
    /// space, under the state lock, like admission does.
    #[tokio::test]
    async fn start_measures_free_space_after_reading_what_is_owed() {
        let rig = Rig::new("start-order");
        let store = rig.open();
        let inner = store.inner.clone();
        let locked = Arc::new(AtomicUsize::new(0));
        let l = locked.clone();
        *rig.budget.on_free.lock().unwrap() = Some(Box::new(move || {
            if inner.state.try_lock().is_err() {
                l.fetch_add(1, Ordering::SeqCst);
            }
        }));
        store.start(request("1")).unwrap();
        // The one reading made by `start` itself, before the task runs.
        assert_eq!(
            locked.load(Ordering::SeqCst),
            1,
            "free read before the owed read"
        );
        store.settled().await;
    }

    /// Starting an item again whose removed folder would not delete: the
    /// folder's bytes stop counting as a tombstone, so the new record
    /// carries them until its plan is admitted.
    #[tokio::test]
    async fn a_restart_over_an_undeleted_folder_carries_its_bytes() {
        let rig = Rig::new("restart-tombstone");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        let left = store.kept_bytes();
        assert!(left > 0);
        store.start(request("1")).unwrap();
        // Before the run's plan is admitted.
        assert_eq!(store.kept_bytes(), left, "the folder counted for less");
        FAIL_REMOVE.with(|f| f.set(false));
        store.settled().await;
    }

    /// Free space is shared too: a second download must leave the guard free
    /// after the first has written what it still has to write.
    #[tokio::test]
    async fn a_second_download_is_held_to_the_free_space_the_first_still_needs() {
        let rig = Rig::new("reserve-free");
        let estimate = estimate_bytes(2_000_000, 12_000);
        // Room for one film beyond the guard, not for two.
        rig.budget
            .free
            .store(FREE_GUARD_BYTES + estimate + 1_000, Ordering::SeqCst);
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        rig.server.asked_times(SEG0, 1).await;
        store.start(request("2")).unwrap();
        let mut waited = 0;
        loop {
            let second = store.videos("u1").into_iter().find(|v| v.item == "2");
            if second
                .as_ref()
                .is_some_and(|v| matches!(v.state, KeptState::Failed { .. }))
            {
                break;
            }
            waited += 1;
            assert!(
                waited < 2_000,
                "the second download was admitted: {second:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        release.store(true, Ordering::SeqCst);
        store.settled().await;
        let videos = store.videos("u1");
        let state = |item: &str| {
            videos
                .iter()
                .find(|v| v.item == item)
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(state("1"), KeptState::Ready);
        let KeptState::Failed { reason } = state("2") else {
            panic!("the second was admitted");
        };
        assert!(reason.contains("guard"), "{reason}");
        let refused = lines("pharos: video keep refused");
        assert!(
            refused.len() == 1
                && refused[0].contains("item=2")
                && refused[0].contains("outstanding="),
            "{refused:?}"
        );
    }

    /// The host counts these bytes against the cap it shares with music:
    /// every user's, a download under way at its whole estimate.
    #[tokio::test]
    async fn the_kept_bytes_cover_every_user_and_a_running_downloads_estimate() {
        let rig = Rig::new("kept-bytes");
        let store = rig.open();
        let mut other = request("2");
        other.user = "u2".into();
        store.start(other).unwrap();
        store.settled().await;
        let ready = store.videos("u2")[0].bytes();
        assert!(ready > 0);
        assert_eq!(store.kept_bytes(), ready);

        // A second user's download stays under way.
        let _held = rig.server.hold_mid_body(SEG0);
        store.start(request("1")).unwrap();
        rig.server.asked_times(SEG0, 2).await;
        let estimate = estimate_bytes(2_000_000, 12_000);
        let mut waited = 0;
        while store.kept_bytes() != ready + estimate {
            waited += 1;
            assert!(waited < 2_000, "kept_bytes={}", store.kept_bytes());
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        // What the running one has yet to write: its estimate less what it
        // has stored whole or staged; the finished one has nothing left.
        let mut waited = 0;
        loop {
            let stored = match store.videos("u1")[0].state {
                KeptState::Downloading { bytes, .. } => bytes,
                ref other => panic!("not running: {other:?}"),
            };
            if store.outstanding_bytes() == estimate - stored - store.staged_bytes() {
                break;
            }
            waited += 1;
            assert!(
                waited < 2_000,
                "outstanding={} estimate={estimate} stored={stored}",
                store.outstanding_bytes()
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    #[tokio::test]
    async fn a_failed_download_leaves_no_staging_file_behind() {
        let rig = Rig::new("parts");
        let _never = rig.server.hold_mid_body(SEG0);
        rig.server.fail(SEG1, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(matches!(only(&store).state, KeptState::Failed { .. }));
        let dir = rig.dir.join("u1").join("1").join("files");
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".part")), "{names:?}");
    }

    /// A retry whose plan is new (a fresh play session names every file
    /// anew) removes the files of the plan it replaces.
    #[tokio::test]
    async fn a_retry_with_a_new_plan_removes_the_old_plans_files() {
        let rig = Rig::new("new-plan");
        let _never = rig.server.hold_mid_body(SEG0);
        rig.server.fail(SEG1, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(matches!(only(&store).state, KeptState::Failed { .. }));
        assert!(
            store.stored_file("u1", INIT).is_some(),
            "the old plan left files"
        );

        // The next attempt negotiates a new session: every key changes.
        let renamed = |t: &str| t.replace("PlaySessionId=p", "PlaySessionId=q");
        {
            let mut f = rig.server.files.lock().unwrap();
            let old: Vec<(String, Vec<u8>)> = f.drain().collect();
            for (key, body) in old {
                let body = match String::from_utf8(body.clone()) {
                    Ok(text) => renamed(&text).into_bytes(),
                    Err(_) => body,
                };
                f.insert(renamed(&key), body);
            }
        }
        rig.server.failures.lock().unwrap().clear();
        let mut again = request("1");
        again.master_url = renamed(MASTER_KEY);
        store.start(again).unwrap();
        store.settled().await;
        let v = only(&store);
        assert_eq!(v.state, KeptState::Ready, "{v:?}");
        assert!(
            store.stored_file("u1", INIT).is_none(),
            "the old file is not indexed"
        );
        let dir = rig.dir.join("u1").join("1").join("files");
        let on_disk = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(on_disk, v.files.len(), "only the new plan's files remain");
        assert_eq!(v.files.len(), 5);
        assert_eq!(lines("pharos: video keep pruned").len(), 1);
    }

    /// A failed attempt leaves files on disk; they stay counted through the
    /// next attempt even when it fails before it has a plan to prune to.
    #[tokio::test]
    async fn a_failed_attempts_files_stay_counted_when_the_retry_fails_in_planning() {
        let rig = Rig::new("retry-fails");
        let _never = rig.server.hold_mid_body(SEG0);
        rig.server.fail(SEG1, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(matches!(only(&store).state, KeptState::Failed { .. }));
        let dir = rig.dir.join("u1").join("1");
        let on_disk = disk_bytes(&dir).bytes;
        assert!(on_disk > 0);
        assert_eq!(
            store.kept_bytes(),
            on_disk,
            "a failed record reports what is on disk"
        );

        // The retry negotiates a session the server cannot answer: it fails
        // reading its master playlist, before any plan.
        let mut again = request("1");
        again.master_url = MASTER_KEY.replace("PlaySessionId=p", "PlaySessionId=q");
        store.start(again).unwrap();
        assert_eq!(
            store.kept_bytes(),
            on_disk,
            "the old files stay counted while the retry starts"
        );
        store.settled().await;
        assert!(matches!(only(&store).state, KeptState::Failed { .. }));
        assert_eq!(
            disk_bytes(&dir).bytes,
            on_disk,
            "nothing was pruned without a plan"
        );
        assert_eq!(store.kept_bytes(), on_disk, "and they are still counted");
    }

    /// A stale file the prune cannot delete still counts against the cap.
    #[tokio::test]
    async fn a_file_the_prune_cannot_delete_is_counted_in_the_ready_record() {
        let rig = Rig::new("prune-fails");
        let _never = rig.server.hold_mid_body(SEG0);
        rig.server.fail(SEG1, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        // A directory where a stale file should be: remove_file refuses it.
        let files = rig.dir.join("u1").join("1").join("files");
        let stuck = files.join(file_name("/stale/from/another/plan.m4s"));
        std::fs::create_dir(&stuck).unwrap();
        std::fs::write(stuck.join("inner"), vec![0u8; 10]).unwrap();

        let renamed = |t: &str| t.replace("PlaySessionId=p", "PlaySessionId=q");
        {
            let mut f = rig.server.files.lock().unwrap();
            let old: Vec<(String, Vec<u8>)> = f.drain().collect();
            for (key, body) in old {
                let body = match String::from_utf8(body.clone()) {
                    Ok(text) => renamed(&text).into_bytes(),
                    Err(_) => body,
                };
                f.insert(renamed(&key), body);
            }
        }
        rig.server.failures.lock().unwrap().clear();
        let mut again = request("1");
        again.master_url = renamed(MASTER_KEY);
        store.start(again).unwrap();
        store.settled().await;
        let v = only(&store);
        assert_eq!(v.state, KeptState::Ready, "{v:?}");
        assert!(stuck.exists(), "the stale entry could not go");
        let planned: u64 = v
            .files
            .iter()
            .map(|k| {
                std::fs::metadata(store.stored_file("u1", k).unwrap().path)
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(
            v.estimated_bytes > planned,
            "the record counts the stuck entry: {} vs {planned}",
            v.estimated_bytes
        );
        assert_eq!(
            v.estimated_bytes,
            disk_bytes(&rig.dir.join("u1").join("1")).bytes
        );
    }

    /// While the retry runs, the stale file the prune could not delete is
    /// part of its reservation, not only of the count after it is done.
    #[tokio::test]
    async fn a_file_the_prune_cannot_delete_stays_in_the_running_downloads_reservation() {
        let rig = Rig::new("prune-fails-running");
        let _never = rig.server.hold_mid_body(SEG0);
        rig.server.fail(SEG1, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let files = rig.dir.join("u1").join("1").join("files");
        let stuck = files.join(file_name("/stale/from/another/plan.m4s"));
        std::fs::create_dir(&stuck).unwrap();
        let stuck_len = std::fs::metadata(&stuck).unwrap().len();
        assert!(stuck_len > 0);

        let renamed = |t: &str| t.replace("PlaySessionId=p", "PlaySessionId=q");
        {
            let mut f = rig.server.files.lock().unwrap();
            let old: Vec<(String, Vec<u8>)> = f.drain().collect();
            for (key, body) in old {
                let body = match String::from_utf8(body.clone()) {
                    Ok(text) => renamed(&text).into_bytes(),
                    Err(_) => body,
                };
                f.insert(renamed(&key), body);
            }
        }
        rig.server.failures.lock().unwrap().clear();
        // The retry stays under way, its first segment held.
        let _held = rig.server.hold_mid_body(&renamed(SEG0));
        let mut again = request("1");
        again.master_url = renamed(MASTER_KEY);
        store.start(again).unwrap();
        rig.server.asked_times(&renamed(SEG0), 1).await;
        let estimate = estimate_bytes(2_000_000, 12_000);
        assert_eq!(
            store.kept_bytes(),
            estimate + stuck_len,
            "the reservation holds the plan and the file that would not go"
        );
    }

    #[tokio::test]
    async fn a_second_start_of_a_kept_item_is_refused() {
        let rig = Rig::new("twice");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(matches!(
            store.start(request("1")),
            Err(KeepVideoRefusal::AlreadyKept { .. })
        ));
    }

    #[tokio::test]
    async fn removing_deletes_the_files_the_index_and_the_record() {
        let rig = Rig::new("remove");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        store.remove("u1", "1");
        assert!(store.videos("u1").is_empty());
        assert!(store.stored_file("u1", SEG0).is_none());
        assert!(!rig.dir.join("u1").join("1").exists());
        let line = lines("pharos: video keep remove");
        assert!(
            line[0].contains("item=1") && line[0].contains("verdict=removed"),
            "{line:?}"
        );
        // And it does not come back after a restart.
        drop(store);
        assert!(rig.open().videos("u1").is_empty());
    }

    /// A folder that will not delete and cannot be measured is not counted
    /// as empty: it keeps the bytes the record reserved, and a later failed
    /// sweep keeps the figure it had.
    #[tokio::test]
    async fn an_unreadable_folder_is_not_counted_as_empty() {
        let rig = Rig::new("tombstone-unreadable");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let reserved = reserved(&only(&store));
        assert!(reserved > 0);

        FAIL_REMOVE.with(|f| f.set(true));
        LEAVE_UNREADABLE.with(|f| f.set(true));
        store.remove("u1", "1");
        LEAVE_UNREADABLE.with(|f| f.set(false));
        assert_eq!(store.kept_bytes(), reserved, "counted as empty");

        // A sweep that fails again over a folder it cannot read keeps it.
        store.inner.sweep_tombstones();
        assert_eq!(store.kept_bytes(), reserved, "the sweep forgot the figure");
        FAIL_REMOVE.with(|f| f.set(false));
    }

    /// Only a missing manifest marks a removed download. One that is there
    /// but cannot be read leaves its folder alone.
    #[tokio::test]
    async fn an_unreadable_manifest_does_not_get_its_folder_deleted() {
        let rig = Rig::new("manifest-unreadable");
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::write(dir.join("files").join("keep"), b"bytes").unwrap();
        // A directory where the manifest should be: reading it fails with
        // something other than "not found".
        std::fs::create_dir(dir.join("manifest.json")).unwrap();
        let store = rig.open();
        assert!(dir.join("files").join("keep").exists(), "the folder went");
        assert!(store.videos("u1").is_empty());
        let held = lines("pharos: video folder held");
        assert!(
            held.len() == 1
                && held[0].contains("reason=record unreadable")
                && held[0].contains("admission=blocked"),
            "{held:?}"
        );
        // The folder's bytes count against the cap, though it is not swept,
        // and nothing is admitted while its record's reservation is lost.
        assert_eq!(store.kept_bytes(), 5, "the held folder counts for nothing");
        let Err(KeepVideoRefusal::Host(why)) = store.start(request("2")) else {
            panic!("admitted beside a folder of unknown size");
        };
        assert!(why.contains("unknown"), "{why}");
        assert!(store.size_unknown(), "the host is not told");
        assert!(dir.join("files").join("keep").exists(), "swept");
    }

    /// A user folder that cannot be listed hides downloads that may be
    /// there: nothing is admitted while it stays.
    #[tokio::test]
    async fn an_unlistable_user_folder_blocks_admission() {
        let rig = Rig::new("user-unlistable");
        let user = rig.dir.join("u9");
        std::fs::create_dir_all(&user).unwrap();
        UNREADABLE.with(|u| u.borrow_mut().push(user.clone()));
        let store = rig.open();
        let held = lines("pharos: video folder unlistable");
        assert!(
            held.len() == 1 && held[0].contains("admission=blocked"),
            "{held:?}"
        );
        assert!(store.size_unknown(), "the host is not told");
        let Err(KeepVideoRefusal::Host(why)) = store.start(request("1")) else {
            panic!("admitted beside a folder that cannot be listed");
        };
        assert!(why.contains("cannot be listed"), "{why}");
        UNREADABLE.with(|u| u.borrow_mut().clear());
    }

    /// A store root that is not a folder cannot be listed either; one that
    /// is not there is an empty store.
    #[tokio::test]
    async fn an_unlistable_root_blocks_admission_and_a_missing_one_is_empty() {
        let rig = Rig::new("root-unlistable");
        // Missing: an empty store that takes downloads.
        let store = rig.open();
        assert!(!store.size_unknown());
        store.start(request("1")).unwrap();
        store.settled().await;
        drop(store);
        // A plain file where the root should be.
        std::fs::remove_dir_all(&rig.dir).unwrap();
        std::fs::write(&rig.dir, b"x").unwrap();
        let store = rig.open();
        assert!(store.size_unknown(), "a root that is a file is not empty");
        assert!(matches!(
            store.start(request("2")),
            Err(KeepVideoRefusal::Host(_))
        ));
        std::fs::remove_file(&rig.dir).unwrap();
    }

    /// An entry that listed but whose lookup then fails is not "not a
    /// folder": its size is unknown and admission is blocked.
    #[tokio::test]
    async fn a_folder_whose_metadata_fails_after_listing_blocks_admission() {
        let rig = Rig::new("entry-metadata");
        let item = rig.dir.join("u9").join("1");
        std::fs::create_dir_all(&item).unwrap();
        // The user folder lists; its entry then cannot be looked up.
        UNLOOKUPABLE.with(|u| u.borrow_mut().push(item.clone()));
        let store = rig.open();
        let held = lines("pharos: video folder unlistable");
        assert!(
            held.len() == 1 && held[0].contains("admission=blocked"),
            "{held:?}"
        );
        assert!(store.size_unknown(), "dropped from the listing");
        UNLOOKUPABLE.with(|u| u.borrow_mut().clear());
        // A dangling link is nothing to list, not an unknown size.
        std::fs::remove_dir_all(&item).unwrap();
        std::os::unix::fs::symlink(rig.dir.join("gone"), &item).unwrap();
        let store = rig.open();
        assert!(!store.size_unknown(), "a dangling link blocked admission");
    }

    /// A chunk counts as staged only once its write has completed: with
    /// the write held pending the bytes are on their way to disk but the
    /// free-space reading does not lack them yet, so they are still owed.
    #[tokio::test]
    async fn a_chunk_is_not_staged_until_its_write_completes() {
        let rig = Rig::new("pending-write");
        HOLD_WRITE.with(|h| h.set(true));
        let store = rig.open();
        store.start(request("1")).unwrap();
        // The first chunk is written and held before its flush.
        let mut waited = 0;
        loop {
            let started = std::fs::read_dir(rig.dir.join("u1").join("1").join("files"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    e.path().extension().is_some_and(|x| x == "part")
                        && e.metadata().is_ok_and(|m| m.len() > 0)
                });
            if started {
                break;
            }
            waited += 1;
            assert!(waited < 2_000, "no write began");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        // Give a counted chunk every chance to show.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            matches!(only(&store).state, KeptState::Downloading { .. }),
            "went on past a write that had not completed: {:?}",
            only(&store).state
        );
        assert_eq!(
            store.staged_bytes(),
            0,
            "counted before the write completed"
        );
        HOLD_WRITE.with(|h| h.set(false));
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
    }

    /// A removal whose folder will not delete and cannot be measured whole
    /// marks the size unknown at once, not at the next sweep.
    #[tokio::test]
    async fn a_removal_that_cannot_measure_the_folder_blocks_admission_at_once() {
        let rig = Rig::new("remove-unmeasured");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        FAIL_REMOVE.with(|f| f.set(true));
        UNREADABLE.with(|u| u.borrow_mut().push(dir.clone()));
        store.remove("u1", "1");
        // Straight after, before any sweep.
        assert!(store.size_unknown(), "known until the next sweep");
        let Err(KeepVideoRefusal::Host(why)) = store.start(request("2")) else {
            panic!("admitted beside a folder of unknown size");
        };
        assert!(why.contains("unknown"), "{why}");
        UNREADABLE.with(|u| u.borrow_mut().clear());
        FAIL_REMOVE.with(|f| f.set(false));
    }

    /// The process's one log capture, for the tests that read the music
    /// cache's log: a logger can be installed once per process.
    fn capture_music_log() -> &'static Mutex<Vec<String>> {
        struct Capture;
        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static INSTALL: std::sync::Once = std::sync::Once::new();
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata) -> bool {
                true
            }
            fn log(&self, r: &log::Record) {
                LINES.lock().unwrap().push(r.args().to_string());
            }
            fn flush(&self) {}
        }
        static CAPTURE: Capture = Capture;
        INSTALL.call_once(|| {
            assert!(
                log::set_logger(&CAPTURE).is_ok(),
                "another logger was installed first"
            );
            log::set_max_level(log::LevelFilter::Info);
        });
        &LINES
    }

    /// The phone registers its video store after creating its folder, and a
    /// folder that cannot be created leaves no store. The hook the music
    /// cache holds then reads the size as unknown, and the cache refuses a
    /// download rather than counting the video as zero. (The phone's
    /// registration is Android-only; the hook's no-store answer, which it
    /// installs before creating the folder, is `ports::outside_bytes(None)`.)
    /// The refusal is read from a real `Library`'s cache on a prefetch,
    /// which is the admission a pin's downloads go through.
    #[test]
    fn a_store_that_could_not_be_created_leaves_music_refused() {
        let rig = Rig::new("no-store");
        std::fs::create_dir_all(&rig.dir).unwrap();
        let root = rig.dir.join("video");
        std::fs::write(&root, b"a file where the folder should be").unwrap();
        assert!(
            std::fs::create_dir_all(&root).is_err(),
            "the folder was made"
        );
        let lines = capture_music_log();
        let base = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let lib = pharos_content::Library::connect_with(
            &base,
            &rig.dir.join("token"),
            pharos_content::Timeouts {
                command: std::time::Duration::from_secs(3),
                code: std::time::Duration::from_millis(800),
            },
        )
        .unwrap();
        lib.set_outside_used(Arc::new(|| crate::native::ports::outside_bytes(None)));
        lib.enable_cache(&rig.dir.join("audio"), 1 << 20).unwrap();
        lib.prefetch(&["track/2".to_owned()]);
        let mut waited = 0;
        let refused = loop {
            let found = lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("reason=outside_size_unknown") && l.contains("track/2"));
            if found || waited > 5_000 {
                break found;
            }
            waited += 1;
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert!(refused, "music was admitted with no video store");
    }

    /// The whole path of the unknown-size block into music: a real store
    /// with a subtree it cannot read, registered as the phone registers it,
    /// the very hook the phone installs (`ports::outside_bytes_hook`), and
    /// a real music download through `Library`, which the cache refuses
    /// before any network use.
    ///
    /// Stubbed: only the server, one nothing listens on. The phone crate is
    /// Android-only and cannot host a test; it registers the same hook.
    #[test]
    fn music_is_refused_through_the_real_hook_while_a_video_subtree_is_unreadable() {
        let lines = capture_music_log();

        // A real store, with its root unreadable the way a restart can find it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let rig = Rig::new("music-refused");
        let user = rig.dir.join("u9");
        std::fs::create_dir_all(&user).unwrap();
        UNREADABLE.with(|u| u.borrow_mut().push(user.clone()));
        let store = {
            let _enter = rt.enter();
            rig.open()
        };
        assert!(store.size_unknown());

        // The music side: a real Library and cache, the phone's hook.
        let base = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let lib = pharos_content::Library::connect_with(
            &base,
            &rig.dir.join("token"),
            pharos_content::Timeouts {
                command: std::time::Duration::from_secs(3),
                code: std::time::Duration::from_millis(800),
            },
        )
        .unwrap();
        // The port slot is set once per process; no other test registers.
        crate::native::ports::register_video_store(store.clone()).unwrap();
        lib.set_outside_used(crate::native::ports::outside_bytes_hook());
        lib.enable_cache(&rig.dir.join("audio"), 1 << 20).unwrap();
        lib.prefetch(&["track/1".to_owned()]);
        let mut waited = 0;
        let refused = loop {
            let found = lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("reason=outside_size_unknown") && l.contains("track/1"));
            if found {
                break true;
            }
            waited += 1;
            if waited > 5_000 {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        UNREADABLE.with(|u| u.borrow_mut().clear());
        assert!(
            refused,
            "music was not refused: {:?}",
            lines.lock().unwrap()
        );
    }

    /// A record that cannot be written is refused at the start, and the
    /// accounting is as it was: nothing runs, the record is gone, and the
    /// one it replaced is back.
    #[tokio::test]
    async fn a_start_whose_record_cannot_be_saved_is_refused_and_rolled_back() {
        let rig = Rig::new("save-fails");
        let store = rig.open();
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(dir.join("manifest.json.part")).unwrap();
        let Err(KeepVideoRefusal::Host(why)) = store.start(request("1")) else {
            panic!("started without a durable record");
        };
        assert!(why.contains("could not be saved"), "{why}");
        assert!(store.videos("u1").is_empty(), "the record stayed");
        assert_eq!(store.kept_bytes(), 0, "its reservation stayed");
        assert!(rig.server.asked().is_empty(), "a run was started");
        let line = lines("pharos: video keep refused");
        assert!(line[0].contains("verdict=rolled_back"), "{line:?}");

        // A failed record the start replaced comes back as it was.
        std::fs::remove_dir_all(dir.join("manifest.json.part")).unwrap();
        rig.server.fail(SEG1, &[403]);
        store.start(request("1")).unwrap();
        store.settled().await;
        let failed = only(&store);
        assert!(
            matches!(failed.state, KeptState::Failed { .. }),
            "{failed:?}"
        );
        std::fs::create_dir_all(dir.join("manifest.json.part")).unwrap();
        assert!(store.start(request("1")).is_err());
        assert_eq!(only(&store), failed, "the replaced record was lost");
    }

    /// A change whose save fails is kept in memory and the error comes
    /// back, so a caller can tell the record is not durable.
    #[tokio::test]
    async fn an_update_whose_save_fails_reports_it_and_keeps_the_change() {
        let rig = Rig::new("update-save-fails");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(dir.join("manifest.json.part")).unwrap();
        let saved = store.inner.update("u1", "1", |v| v.position_ms = 4_200);
        assert!(saved.is_err(), "a failed save went unreported");
        assert_eq!(only(&store).position_ms, 4_200, "the change was undone");
        assert!(!lines("pharos: video record not saved").is_empty());
        // Gone is not a failure.
        assert!(matches!(
            store.inner.update("u1", "nope", |_| {}),
            Ok(false)
        ));
        // Once the disk is well again the next save writes it.
        std::fs::remove_dir_all(dir.join("manifest.json.part")).unwrap();
        assert!(matches!(store.inner.update("u1", "1", |_| {}), Ok(true)));
        drop(store);
        assert_eq!(rig.open().videos("u1")[0].position_ms, 4_200);
    }

    /// An admission made while a chunk's write is pending: the pending
    /// bytes are not on the disk's books yet, so the first download still
    /// owes them and a second that fits only if they were counted as
    /// already written is refused.
    #[tokio::test]
    async fn an_admission_during_a_pending_write_still_counts_those_bytes_as_owed() {
        let rig = Rig::new("pending-admission");
        let estimate = estimate_bytes(2_000_000, 12_000);
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        // The first download is admitted and parked in SEG0 with 50 staged.
        // (Everything else stored whole: a playlist's first half can also
        // be 50 staged for an instant.)
        let stored = (MASTER.len() + MEDIA.len() + 10 + 100) as u64;
        let mut waited = 0;
        loop {
            let bytes = match store.videos("u1").first().map(|v| v.state.clone()) {
                Some(KeptState::Downloading { bytes, .. }) => bytes,
                _ => 0,
            };
            if bytes == stored && store.staged_bytes() == 50 {
                break;
            }
            waited += 1;
            assert!(
                waited < 2_000,
                "bytes={bytes} staged={}",
                store.staged_bytes()
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        // From here its writes are held pending after `write_all`.
        HOLD_ITEM.with(|h| *h.borrow_mut() = Some("1".to_owned()));
        release.store(true, Ordering::SeqCst);
        let mut waited = 0;
        loop {
            let pending = std::fs::read_dir(rig.dir.join("u1").join("1").join("files"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    e.path().extension().is_some_and(|x| x == "part")
                        && e.metadata().is_ok_and(|m| m.len() > 50)
                });
            if pending {
                break;
            }
            waited += 1;
            assert!(waited < 2_000, "no pending write");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(store.staged_bytes(), 50, "the pending bytes were staged");

        // Room for a second film only if the pending bytes were not owed:
        // one byte short with them owed.
        // The owed figure worked out from the plan, not read from the
        // store: whole files stored and the 50 staged, nothing pending.
        let owed = estimate - stored - 50;
        rig.budget
            .free
            .store(owed + estimate + FREE_GUARD_BYTES - 1, Ordering::SeqCst);
        store.start(request("2")).unwrap();
        let mut waited = 0;
        let second = loop {
            let second = store.videos("u1").into_iter().find(|v| v.item == "2");
            if let Some(v) = second
                .filter(|v| v.estimated_bytes > 0 || matches!(v.state, KeptState::Failed { .. }))
            {
                break v;
            }
            waited += 1;
            assert!(waited < 2_000, "the second never settled");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        };
        HOLD_ITEM.with(|h| *h.borrow_mut() = None);
        let KeptState::Failed { reason } = &second.state else {
            panic!("admitted on room the pending write still owes: {second:?}");
        };
        assert!(reason.contains("guard"), "{reason}");
        store.settled().await;
    }

    /// While a removed download's folder is being deleted and measured its
    /// size is not known: an admission in that window is refused, not let
    /// through on a reservation that is only an estimate.
    #[tokio::test]
    async fn an_admission_while_a_folder_is_being_removed_is_refused() {
        let rig = Rig::new("remove-window");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let (s2, seen2) = (store.clone(), seen.clone());
        // Read inside the deletion, where no start can run (the removal
        // holds the saves lock a start's save would wait on): the state
        // every admission reads.
        DURING_REMOVE.with(|d| {
            *d.borrow_mut() = Some(Box::new(move || {
                seen2.set(Some((
                    s2.size_unknown(),
                    lock(&s2.inner.state).unknown_folder(),
                )));
            }));
        });
        store.remove("u1", "1");
        DURING_REMOVE.with(|d| *d.borrow_mut() = None);
        let (unknown, refusal) = seen.take().unwrap();
        assert!(unknown, "known while the folder was being deleted");
        assert!(
            matches!(refusal, Some(KeepVideoRefusal::Host(_))),
            "{refusal:?}"
        );
        // Once it is gone the block lifts.
        assert!(!store.size_unknown(), "still blocked after the removal");
    }

    /// Ready is seen only once it is on disk: a `videos()` read while the
    /// Ready record is being saved still shows the download as running.
    #[tokio::test]
    async fn ready_is_not_visible_until_its_record_is_saved() {
        let rig = Rig::new("ready-after-save");
        let store = rig.open();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (s2, seen2) = (store.clone(), seen.clone());
        DURING_SAVE.with(|d| {
            *d.borrow_mut() = Some(Box::new(move |saving: &KeptVideo| {
                if saving.ready() {
                    seen2
                        .borrow_mut()
                        .push(s2.videos("u1").iter().any(KeptVideo::ready));
                }
            }));
        });
        store.start(request("1")).unwrap();
        store.settled().await;
        DURING_SAVE.with(|d| *d.borrow_mut() = None);
        assert_eq!(
            *seen.borrow(),
            vec![false],
            "Ready seen before it was saved"
        );
        assert!(only(&store).ready(), "never became Ready");
        // And the disk agrees: a reopen finds it Ready.
        drop(store);
        assert!(only(&rig.open()).ready());
    }

    /// A download whose terminal record cannot be saved is not reported
    /// Ready: that would resume at the next open. It is reported failed,
    /// with the save error as the reason, and no success is logged.
    #[tokio::test]
    async fn a_completion_whose_record_cannot_be_saved_is_not_reported_ready() {
        let rig = Rig::new("complete-save-fails");
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        let mut waited = 0;
        while store.staged_bytes() != 50 {
            waited += 1;
            assert!(waited < 2_000, "staged={}", store.staged_bytes());
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        // The disk goes bad while the download is mid-file.
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(dir.join("manifest.json.part")).unwrap();
        release.store(true, Ordering::SeqCst);
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("reported {:?}", only(&store).state);
        };
        assert!(reason.contains("could not be saved"), "{reason}");
        assert!(
            lines("pharos: video keep ready").is_empty(),
            "success logged"
        );
        let failed = lines("pharos: video keep failed");
        assert!(
            failed.len() == 1 && failed[0].contains("could not be saved"),
            "{failed:?}"
        );
        // The disk disagrees with nothing: it holds no Ready record.
        std::fs::remove_dir_all(dir.join("manifest.json.part")).unwrap();
        drop(store);
        let reopened = rig.open();
        assert_ne!(only(&reopened).state, KeptState::Ready);
    }

    /// The Failed record a failed completion save leaves still reserves what
    /// the folder holds, when that is more than was planned.
    #[tokio::test]
    async fn a_failed_completion_save_keeps_what_the_folder_holds() {
        let rig = Rig::new("complete-save-fails-bytes");
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        let mut waited = 0;
        while store.staged_bytes() != 50 {
            waited += 1;
            assert!(waited < 2_000, "staged={}", store.staged_bytes());
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let dir = rig.dir.join("u1").join("1");
        let big = estimate_bytes(2_000_000, 12_000) * 3;
        let stray = std::fs::File::create(dir.join("files").join("stray")).unwrap();
        stray.set_len(big).unwrap();
        std::fs::create_dir_all(dir.join("manifest.json.part")).unwrap();
        release.store(true, Ordering::SeqCst);
        store.settled().await;
        assert!(matches!(only(&store).state, KeptState::Failed { .. }));
        assert!(
            store.kept_bytes() >= big,
            "reserves {} of the {big} the folder holds",
            store.kept_bytes()
        );
    }

    /// A plan whose record cannot be saved at admission stops the download
    /// with the save error, before any file is fetched.
    #[tokio::test]
    async fn a_plan_whose_record_cannot_be_saved_stops_the_download() {
        let rig = Rig::new("plan-save-fails");
        let store = rig.open();
        store.start(request("1")).unwrap();
        // The run has not begun: the disk goes bad between start and plan.
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(dir.join("manifest.json.part")).unwrap();
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("reported {:?}", only(&store).state);
        };
        assert!(
            reason.contains("plan's record could not be saved"),
            "{reason}"
        );
        assert!(
            !rig.server.asked().iter().any(|k| k == SEG0),
            "fetched on after the plan was lost"
        );
    }

    /// A refused plan's cleanup deletes only while its run owns the item: a
    /// replacement run's file at the same path stays.
    #[tokio::test]
    async fn a_stale_runs_refusal_cleanup_leaves_the_replacements_files() {
        let rig = Rig::new("refused-cleanup");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let key = SEG0.to_owned();
        let path = store.stored_file("u1", &key).unwrap().path;
        let owner = lock(&store.inner.state)
            .runs
            .get(&("u1".to_owned(), "1".to_owned()))
            .copied();
        // The run that finished no longer owns the item (its entry is gone),
        // as after a removal and restart: nothing is deleted.
        assert!(owner.is_none());
        drop_refused_files(&store.inner, "u1", "1", 999, std::slice::from_ref(&key));
        assert!(path.exists(), "a stale run deleted the replacement's file");
        assert!(store.stored_file("u1", &key).is_some(), "and forgot it");
        // The owning run deletes and forgets.
        lock(&store.inner.state)
            .runs
            .insert(("u1".to_owned(), "1".to_owned()), 999);
        drop_refused_files(&store.inner, "u1", "1", 999, std::slice::from_ref(&key));
        assert!(!path.exists());
        assert!(store.stored_file("u1", &key).is_none());
    }

    /// Downloads are served to the user who kept them: another account
    /// making the same request gets no stored answer, and neither does
    /// nobody; the files stay for their owner.
    #[tokio::test]
    async fn a_stored_file_is_served_only_to_its_own_user() {
        let rig = Rig::new("scoped-lookup");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(store.stored_file("u1", SEG0).is_some(), "the owner");
        assert!(store.stored_file("u2", SEG0).is_none(), "another account");
        assert!(store.stored_file("", SEG0).is_none(), "nobody");
        // It survives a reopen for its owner, and still not for others.
        drop(store);
        let store = rig.open();
        assert!(store.stored_file("u1", SEG0).is_some());
        assert!(store.stored_file("u2", SEG0).is_none());
    }

    /// A damaged manifest is held the same way.
    #[tokio::test]
    async fn a_damaged_manifest_folder_counts_and_blocks_admission() {
        let rig = Rig::new("manifest-damaged");
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"{ not json").unwrap();
        let store = rig.open();
        assert_eq!(store.kept_bytes(), 10);
        assert!(matches!(
            store.start(request("2")),
            Err(KeepVideoRefusal::Host(_))
        ));
    }

    /// A restart while a removed download's folder will not delete and
    /// cannot be fully read: the figure from before the restart is gone, so
    /// nothing is admitted until a sweep deletes it.
    #[tokio::test]
    async fn a_restart_over_a_folder_that_will_not_delete_and_is_unreadable_blocks_admission() {
        let rig = Rig::new("reopen-unreadable");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        drop(store);

        UNREADABLE.with(|u| u.borrow_mut().push(dir.clone()));
        let store = rig.open();
        let line = lines("pharos: video keep tombstone dir=");
        assert!(line.iter().any(|l| l.contains("exact=false")), "{line:?}");
        let Err(KeepVideoRefusal::Host(why)) = store.start(request("2")) else {
            panic!("admitted beside a folder of unknown size");
        };
        assert!(why.contains("unknown"), "{why}");
        assert!(store.size_unknown(), "the host is not told");

        // Once the folder can be deleted, the sweep clears the block.
        UNREADABLE.with(|u| u.borrow_mut().clear());
        FAIL_REMOVE.with(|f| f.set(false));
        store.start(request("2")).unwrap();
        store.settled().await;
        assert!(!dir.exists(), "the sweep deleted it");
        assert_eq!(store.videos("u1")[0].state, KeptState::Ready);
        assert!(!store.size_unknown(), "still blocked");
    }

    /// A manifest that cannot be unlinked would bring the download back at
    /// the next open: the removal is reported as not done and the record
    /// stays.
    #[tokio::test]
    async fn a_failed_manifest_unlink_leaves_the_record_in_place() {
        let rig = Rig::new("unlink-fails");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        // Unlinking a non-empty directory fails with something other than
        // "not found".
        std::fs::remove_file(dir.join("manifest.json")).unwrap();
        std::fs::create_dir(dir.join("manifest.json")).unwrap();
        std::fs::write(dir.join("manifest.json").join("x"), b"x").unwrap();

        store.remove("u1", "1");
        assert_eq!(store.videos("u1").len(), 1, "the record went");
        assert!(store.stored_file("u1", SEG0).is_some(), "its files went");
        assert!(dir.join("files").exists(), "the folder went");
        let line = lines("pharos: video keep remove");
        assert!(line[0].contains("verdict=not_removed"), "{line:?}");
    }

    /// A sweep that runs while a removal is deleting its folder leaves the
    /// folder, its tombstone and its mark alone: the mark clears only when
    /// the removal itself is done.
    #[tokio::test]
    async fn a_sweep_during_a_removal_leaves_its_folder_and_mark() {
        let rig = Rig::new("sweep-during-remove");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let (s2, seen2, dir2) = (store.clone(), seen.clone(), dir.clone());
        let entered = std::cell::Cell::new(false);
        DURING_REMOVE.with(|d| {
            *d.borrow_mut() = Some(Box::new(move || {
                if entered.replace(true) {
                    return;
                }
                s2.inner.sweep_tombstones();
                seen2.set(Some((
                    s2.size_unknown(),
                    s2.kept_bytes(),
                    dir2.join("files").exists(),
                )));
            }));
        });
        store.remove("u1", "1");
        DURING_REMOVE.with(|d| *d.borrow_mut() = None);
        let (unknown, kept, exists) = seen.take().unwrap();
        assert!(unknown, "a sweep cleared the mark mid-removal");
        assert!(kept > 0, "a sweep dropped the tombstone mid-removal");
        assert!(exists, "a sweep deleted the folder mid-removal");
        assert!(!store.size_unknown(), "still blocked after the removal");
    }

    /// When the manifest unlink fails, the tombstone and the mark stay until
    /// the record is back: no moment in between where an admission sees
    /// neither.
    #[tokio::test]
    async fn a_failed_unlink_restores_the_record_and_drops_the_tombstone_together() {
        let rig = Rig::new("unlink-fails-gap");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let kept = store.kept_bytes();
        assert!(kept > 0);
        let dir = rig.dir.join("u1").join("1");
        std::fs::remove_file(dir.join("manifest.json")).unwrap();
        std::fs::create_dir(dir.join("manifest.json")).unwrap();
        std::fs::write(dir.join("manifest.json").join("x"), b"x").unwrap();
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let (s2, seen2) = (store.clone(), seen.clone());
        AT_UNLINK_FAILURE.with(|d| {
            *d.borrow_mut() = Some(Box::new(move || {
                seen2.set(Some((s2.kept_bytes(), s2.size_unknown())));
            }));
        });
        store.remove("u1", "1");
        AT_UNLINK_FAILURE.with(|d| *d.borrow_mut() = None);
        let (during, unknown) = seen.take().unwrap();
        assert_eq!(
            during, kept,
            "counted for nothing before the record is back"
        );
        assert!(unknown, "no mark before the record is back");
        assert_eq!(store.videos("u1").len(), 1, "the record did not come back");
        assert_eq!(store.kept_bytes(), kept, "counted twice or not at all");
        assert!(!store.size_unknown(), "the mark outlived the restore");
    }

    /// A run that a removal has revoked cannot recreate its folder or its
    /// staging file.
    #[tokio::test]
    async fn a_revoked_run_cannot_recreate_its_folder() {
        let rig = Rig::new("revoked-create");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        store.remove("u1", "1");
        let dir = rig.dir.join("u1").join("1");
        let path = store.inner.file_path("u1", "1", SEG0);
        let out = fetch_once(&store.inner, "u1", "1", 1, SEG0, &path).await;
        assert!(matches!(out, Err(FetchOnce::Gone)), "{}", out.is_ok());
        assert!(!dir.exists(), "a revoked run brought the folder back");
    }

    /// A removal whose folder will not delete, while the revoked run's
    /// staging file is still open: the folder's size is not bounded until
    /// the writer stops, so it stays unknown and is measured (here deleted)
    /// only then.
    #[tokio::test]
    async fn a_folder_with_a_running_writer_stays_unknown_until_the_writer_stops() {
        let rig = Rig::new("writer-running");
        let release = rig.server.hold_mid_body(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        let mut waited = 0;
        while store.staged_bytes() == 0 {
            waited += 1;
            assert!(waited < 2_000, "the write never started");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        FAIL_REMOVE.with(|f| f.set(false));
        assert!(
            store.size_unknown(),
            "measured while a writer could still grow the folder"
        );
        // The writer is let go, sees it is revoked and stops; the sweep at
        // its end deletes the folder and the block lifts.
        release.store(true, Ordering::SeqCst);
        let mut waited = 0;
        while store.size_unknown() || store.kept_bytes() > 0 {
            waited += 1;
            assert!(
                waited < 2_000,
                "unknown={} kept={}",
                store.size_unknown(),
                store.kept_bytes()
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            !rig.dir.join("u1").join("1").exists(),
            "the folder was not deleted once the writer stopped"
        );
    }

    /// With the music cache not up, the shared cap cannot be checked: a new
    /// download is refused, and starts again once it is.
    #[tokio::test]
    async fn a_start_is_refused_until_the_shared_accounting_is_ready() {
        let rig = Rig::new("accounting-not-ready");
        rig.budget.ready.store(false, Ordering::SeqCst);
        let store = rig.open();
        let refusal = store.start(request("1")).unwrap_err();
        assert!(matches!(refusal, KeepVideoRefusal::Host(_)), "{refusal:?}");
        assert!(store.videos("u1").is_empty());
        assert!(rig.server.asked().is_empty());
        rig.budget.ready.store(true, Ordering::SeqCst);
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(only(&store).ready());
    }

    /// A download that restarts with no plan and none of its files is fresh:
    /// it passes the same readiness gate as a start, or it would be admitted
    /// against the default cap and no music.
    #[tokio::test]
    async fn a_restarted_download_with_no_plan_waits_for_the_shared_accounting() {
        let rig = Rig::new("fresh-not-ready");
        let store = rig.open();
        store.start(request("1")).unwrap();
        // The music cache went away between the start and the run's plan.
        rig.budget.ready.store(false, Ordering::SeqCst);
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("a fresh plan was admitted without the shared accounting");
        };
        assert!(reason.contains("music cache is not up"), "{reason}");
        assert_eq!(store.kept_bytes(), 0);
    }

    /// The music library is published while a fresh plan is being admitted,
    /// after the figures were read: the plan is refused, not checked against
    /// the default cap and no music.
    #[tokio::test]
    async fn a_library_published_during_admission_does_not_pass_a_stale_check() {
        let rig = Rig::new("publish-during-run");
        let store = rig.open();
        store.start(request("1")).unwrap();
        rig.budget.ready.store(false, Ordering::SeqCst);
        let b = rig.budget.clone();
        *rig.budget.on_free.lock().unwrap() = Some(Box::new(move || {
            // The library goes up, with the real (tiny) cap.
            b.cap.store(1, Ordering::SeqCst);
            b.ready.store(true, Ordering::SeqCst);
        }));
        store.settled().await;
        *rig.budget.on_free.lock().unwrap() = None;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("the plan was admitted against the default cap");
        };
        assert!(reason.contains("music cache is not up"), "{reason}");
    }

    /// The same for a start.
    #[tokio::test]
    async fn a_library_published_during_a_start_does_not_pass_a_stale_check() {
        let rig = Rig::new("publish-during-start");
        rig.budget.ready.store(false, Ordering::SeqCst);
        let store = rig.open();
        let b = rig.budget.clone();
        *rig.budget.on_free.lock().unwrap() = Some(Box::new(move || {
            b.cap.store(1, Ordering::SeqCst);
            b.ready.store(true, Ordering::SeqCst);
        }));
        let refusal = store.start(request("1")).unwrap_err();
        *rig.budget.on_free.lock().unwrap() = None;
        assert!(matches!(refusal, KeepVideoRefusal::Host(_)), "{refusal:?}");
        assert!(store.videos("u1").is_empty());
    }

    /// An interrupted download reopened with no plan and none of its files
    /// is fresh: reopened with the accounting not up, it waits for it.
    #[tokio::test]
    async fn a_reopened_interrupted_download_with_no_plan_waits_for_the_accounting() {
        let rig = Rig::new("reopen-no-plan");
        let first = rig.open();
        first.start(request("1")).unwrap();
        // Closed before the run planned anything.
        for t in first.inner.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        drop(first);
        rig.budget.ready.store(false, Ordering::SeqCst);
        let store = rig.open();
        store.settled().await;
        let KeptState::Failed { reason } = only(&store).state else {
            panic!("the restarted download was admitted without the accounting");
        };
        assert!(reason.contains("music cache is not up"), "{reason}");
    }

    /// A walk of the folder that comes back short (a transient EMFILE) must
    /// not persist as an empty folder: the record keeps its figure, and the
    /// folder is unknown until a sweep measures it whole.
    #[tokio::test]
    async fn a_short_walk_of_the_folder_does_not_record_it_as_empty() {
        let rig = Rig::new("short-walk");
        let store = rig.open();
        let dir = rig.dir.join("u1").join("1");
        // The sweep's measure is unreadable too, so the fact stays.
        UNREADABLE.with(|u| u.borrow_mut().push(dir.clone()));
        FAIL_DISK_WALK.with(|f| f.set(true));
        store.start(request("1")).unwrap();
        store.settled().await;
        FAIL_DISK_WALK.with(|f| f.set(false));
        assert!(only(&store).ready());
        assert!(
            store.kept_bytes() > 0,
            "the record fell to {} after a short walk",
            store.kept_bytes()
        );
        assert!(store.size_unknown(), "a short walk left the folder known");
        UNREADABLE.with(|u| u.borrow_mut().clear());
    }

    /// A download bigger than its estimate, finished while the walk of its
    /// folder falls short: the record keeps what it downloaded (not its
    /// estimate), the folder is unknown before the record is published, and
    /// the figure survives a reopen.
    #[tokio::test]
    async fn a_download_above_its_estimate_keeps_its_bytes_after_a_short_walk() {
        let rig = Rig::new("short-walk-big");
        rig.server
            .files
            .lock()
            .unwrap()
            .insert(SEG0.into(), vec![2; 20_000_000]);
        let store = rig.open();
        let dir = rig.dir.join("u1").join("1");
        UNREADABLE.with(|u| u.borrow_mut().push(dir.clone()));
        FAIL_DISK_WALK.with(|f| f.set(true));
        let (s2, d2) = (store.clone(), dir.clone());
        let unknown_first = std::rc::Rc::new(std::cell::Cell::new(None));
        let u2 = unknown_first.clone();
        DURING_SAVE.with(|d| {
            *d.borrow_mut() = Some(Box::new(move |v: &KeptVideo| {
                if v.ready() {
                    u2.set(Some(lock(&s2.inner.state).leftover.contains(&d2)));
                }
            }));
        });
        let mut req = request("1");
        req.runtime_ms = 1_000;
        store.start(req).unwrap();
        store.settled().await;
        FAIL_DISK_WALK.with(|f| f.set(false));
        DURING_SAVE.with(|d| *d.borrow_mut() = None);
        assert!(only(&store).ready());
        assert_eq!(
            unknown_first.get(),
            Some(true),
            "the record was published before the folder was marked unknown"
        );
        assert!(
            store.kept_bytes() >= 20_000_000,
            "the record fell to {}",
            store.kept_bytes()
        );
        drop(store);
        UNREADABLE.with(|u| u.borrow_mut().clear());
        let reopened = rig.open();
        assert!(
            reopened.kept_bytes() >= 20_000_000,
            "the figure did not survive a reopen: {}",
            reopened.kept_bytes()
        );
    }

    /// A staging file that cannot be deleted does not drop out of the
    /// accounting: with the manifest unlink failing too, the folder stays
    /// unknown after the writer has stopped, until a later sweep deletes the
    /// file and the folder is measured whole.
    #[tokio::test]
    async fn a_staging_file_that_will_not_delete_keeps_the_folder_unknown_until_a_sweep() {
        let rig = Rig::new("part-stays");
        let store = rig.open();
        let release = park_writer(&rig, &store).await;
        let dir = rig.dir.join("u1").join("1");
        std::fs::remove_file(dir.join("manifest.json")).ok();
        std::fs::create_dir(dir.join("manifest.json")).unwrap();
        std::fs::write(dir.join("manifest.json").join("x"), b"x").unwrap();
        FAIL_PART_DELETE.with(|f| f.set(true));
        store.remove("u1", "1");
        assert_eq!(store.videos("u1").len(), 1, "the record did not come back");
        // The revoked run's file grows after the record was put back.
        let big = estimate_bytes(2_000_000, 12_000) * 3;
        for e in std::fs::read_dir(dir.join("files")).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "part") {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(e.path())
                    .unwrap();
                f.set_len(big).unwrap();
            }
        }
        release.store(true, Ordering::SeqCst);
        // The writer stops; its file will not go.
        let mut waited = 0;
        while lock(&store.inner.state).writing(&dir) {
            waited += 1;
            assert!(waited < 2_000, "the writer never stopped");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        store.settled().await;
        let parts = || {
            std::fs::read_dir(dir.join("files"))
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
                .count()
        };
        assert!(parts() > 0, "the file was deleted anyway");
        // Counted or unknown, never uncounted: what the file holds is in the
        // figure once the folder has been read whole.
        assert!(
            store.size_unknown() || store.kept_bytes() >= big,
            "the leftover file went uncounted: kept {}",
            store.kept_bytes()
        );
        // Later the file goes and the record can be written again, and a
        // sweep resolves the folder.
        FAIL_PART_DELETE.with(|f| f.set(false));
        std::fs::remove_dir_all(dir.join("manifest.json")).unwrap();
        store.inner.sweep_tombstones();
        assert!(!store.size_unknown(), "the sweep did not resolve it");
        assert!(
            store.kept_bytes() >= big,
            "the bytes dropped out of the figure"
        );
        // (The file itself is deleted at the next open, which clears staging
        // files; the figure keeps what it held.)
    }

    /// A tombstoned folder with a leftover and a big file in it, for the
    /// settlement tests; returns the store and the folder.
    async fn tomb_with_leftover(name: &str) -> (Rig, Arc<DiskVideoStore>, PathBuf) {
        let rig = Rig::new(name);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        let dir = rig.dir.join("u1").join("1");
        lock(&store.inner.state).leftover.push(dir.clone());
        (rig, store, dir)
    }

    type Hook = std::thread::LocalKey<RefCell<Option<Box<dyn Fn()>>>>;

    fn set_hook(hook: &'static Hook, f: impl Fn() + 'static) {
        hook.with(|d| *d.borrow_mut() = Some(Box::new(f)));
    }

    fn clear_hook(hook: &'static Hook) {
        hook.with(|d| *d.borrow_mut() = None);
    }

    /// A walk that a writer's start and stop overtook is not taken: the
    /// leftover stays until a walk made after the last change.
    #[tokio::test]
    async fn a_settlement_overtaken_by_a_change_applies_nothing() {
        let (_rig, store, dir) = tomb_with_leftover("settle-stale").await;
        let s2 = store.clone();
        let d2 = dir.clone();
        let once = std::cell::Cell::new(false);
        set_hook(&DURING_MEASURE, move || {
            if !once.replace(true) {
                lock(&s2.inner.state).bump(&d2);
            }
        });
        let before = store.kept_bytes();
        store.inner.settle_leftovers();
        clear_hook(&DURING_MEASURE);
        assert!(
            lock(&store.inner.state).leftover.contains(&dir),
            "a stale walk cleared the leftover"
        );
        assert_eq!(
            store.kept_bytes(),
            before,
            "a stale walk changed the figure"
        );
        store.inner.settle_leftovers();
        assert!(
            !lock(&store.inner.state).leftover.contains(&dir),
            "a walk made after the change did not settle it"
        );
        FAIL_REMOVE.with(|f| f.set(false));
    }

    /// While one sweep settles a folder, a second sweep does not, and a
    /// start in it is refused: the first sweep's view cannot go stale under
    /// a settlement and a retry.
    #[tokio::test]
    async fn one_settlement_per_folder_and_a_start_waits_for_it() {
        let (_rig, store, dir) = tomb_with_leftover("settle-once").await;
        let s2 = store.clone();
        let d2 = dir.clone();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen2 = seen.clone();
        set_hook(&BEFORE_SETTLE_DROP, move || {
            s2.inner.settle_leftovers();
            seen2
                .borrow_mut()
                .push(lock(&s2.inner.state).leftover.contains(&d2));
            seen2.borrow_mut().push(s2.start(request("1")).is_err());
        });
        store.inner.settle_leftovers();
        clear_hook(&BEFORE_SETTLE_DROP);
        assert_eq!(
            *seen.borrow(),
            vec![true, true],
            "the nested sweep settled the folder, or the start got in"
        );
        assert!(lock(&store.inner.state).settling.is_empty());
        FAIL_REMOVE.with(|f| f.set(false));
    }

    /// A staging file that appears after a settlement began (a retry's) is
    /// not deleted by it: ownership and the epoch are checked again before
    /// each file goes.
    #[tokio::test]
    async fn a_settlement_never_deletes_a_file_made_after_it_began() {
        let (_rig, store, dir) = tomb_with_leftover("settle-drop").await;
        let (s2, d2) = (store.clone(), dir.clone());
        let fresh = dir.join("files").join("new.9.part");
        let f2 = fresh.clone();
        set_hook(&BEFORE_SETTLE_DROP, move || {
            std::fs::create_dir_all(d2.join("files")).unwrap();
            std::fs::write(&f2, b"retry").unwrap();
            lock(&s2.inner.state).bump(&d2);
        });
        store.inner.settle_leftovers();
        clear_hook(&BEFORE_SETTLE_DROP);
        assert!(fresh.exists(), "the retry's staging file was deleted");
        assert!(lock(&store.inner.state).leftover.contains(&dir));
        FAIL_REMOVE.with(|f| f.set(false));
    }

    /// The larger reservation is saved before the fact clears; a failed
    /// save keeps the leftover.
    #[tokio::test]
    async fn a_leftover_clears_only_once_the_reservation_is_saved() {
        let rig = Rig::new("settle-save");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(dir.join("files")).unwrap();
        let big = estimate_bytes(2_000_000, 12_000) * 3;
        std::fs::write(
            dir.join("big.bin"),
            vec![0u8; usize::try_from(big).unwrap()],
        )
        .unwrap();
        lock(&store.inner.state).leftover.push(dir.clone());
        // The manifest cannot be replaced.
        std::fs::remove_file(dir.join("manifest.json")).unwrap();
        std::fs::create_dir(dir.join("manifest.json")).unwrap();
        std::fs::write(dir.join("manifest.json").join("x"), b"x").unwrap();
        store.inner.settle_leftovers();
        assert!(
            lock(&store.inner.state).leftover.contains(&dir),
            "the leftover cleared with the save failing"
        );
        std::fs::remove_dir_all(dir.join("manifest.json")).unwrap();
        store.inner.settle_leftovers();
        assert!(!lock(&store.inner.state).leftover.contains(&dir));
        let on_disk: KeptVideo =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert!(
            on_disk.estimated_bytes >= big,
            "the new reservation is not on disk"
        );
    }

    /// At open, a staging file that will not go keeps the folder unknown.
    #[tokio::test]
    async fn a_reopen_with_an_undeletable_staging_file_keeps_the_folder_unknown() {
        let rig = Rig::new("reopen-part");
        {
            let store = rig.open();
            store.start(request("1")).unwrap();
            store.settled().await;
        }
        let dir = rig.dir.join("u1").join("1");
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::write(dir.join("files").join("a.part"), b"x").unwrap();
        FAIL_PART_DELETE.with(|f| f.set(true));
        let store = rig.open();
        assert!(store.size_unknown(), "the folder was read as known");
        FAIL_PART_DELETE.with(|f| f.set(false));
        store.inner.sweep_tombstones();
        assert!(!store.size_unknown(), "the sweep did not resolve it");
    }

    /// A directory entry that cannot be read may be a staging file: the
    /// clean-up is not clean.
    #[test]
    fn an_unreadable_entry_makes_the_clean_up_not_clean() {
        let dir = std::env::temp_dir().join(format!("kvd-entry-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::write(dir.join("files").join("a.part"), b"x").unwrap();
        FAIL_ENTRY_READ.with(|f| f.set(true));
        let clean = drop_parts_of(&dir, None);
        FAIL_ENTRY_READ.with(|f| f.set(false));
        std::fs::remove_dir_all(&dir).ok();
        assert!(!clean, "an entry read error counted as clean");
    }

    /// A same-item retry takes over a folder that would not delete: the
    /// record absorbs the tombstone, so the retry's writes do not leave the
    /// folder "unknown" for every other admission, and its floor is kept.
    #[tokio::test]
    async fn a_retry_over_an_undeletable_folder_absorbs_it_and_blocks_nothing() {
        let rig = Rig::new("takeover");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        let floor = store.kept_bytes();
        assert!(floor > 0, "no tombstone");
        assert!(!store.size_unknown(), "the folder was not measured");
        // The delete keeps failing and part of what was there is gone, so
        // the retry has files to fetch again: it takes the folder over and
        // writes (a writer starting and stopping moves the folder's epoch).
        let dir = rig.dir.join("u1").join("1");
        for e in std::fs::read_dir(dir.join("files")).unwrap().flatten() {
            std::fs::remove_file(e.path()).unwrap();
        }
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(only(&store).ready());
        assert!(
            !store.size_unknown(),
            "the folder a record owns is still unknown"
        );
        assert!(
            store.kept_bytes() >= floor.min(disk_bytes(&dir).bytes),
            "the folder's bytes are not counted"
        );
        // Another item is admitted.
        store.start(request("2")).unwrap();
        store.settled().await;
        FAIL_REMOVE.with(|f| f.set(false));
        assert_eq!(store.videos("u1").len(), 2);
    }

    /// The retry is a new play session: every file is named afresh, the old
    /// session's files are still on disk when it starts, and they are
    /// counted until the plan prunes them. Nothing is refused forever.
    #[tokio::test]
    async fn a_retry_in_a_new_session_over_an_undeletable_folder_counts_the_old_files() {
        let rig = Rig::new("takeover-new-session");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        let dir = rig.dir.join("u1").join("1");
        let old = disk_bytes(&dir).bytes;
        assert!(old > 0, "no old files");
        // The same stream under a new session id.
        {
            let mut files = rig.server.files.lock().unwrap();
            let renamed: Vec<(String, Vec<u8>)> = files
                .iter()
                .map(|(k, v)| {
                    let body = match String::from_utf8(v.clone()) {
                        Ok(text) if text.starts_with("#EXTM3U") => text
                            .replace("PlaySessionId=p", "PlaySessionId=q")
                            .into_bytes(),
                        _ => v.clone(),
                    };
                    (k.replace("PlaySessionId=p", "PlaySessionId=q"), body)
                })
                .collect();
            files.extend(renamed);
        }
        let mut req = request("1");
        req.master_url = MASTER_KEY.replace("PlaySessionId=p", "PlaySessionId=q");
        store.start(req).unwrap();
        // Before the run plans anything, the old files are in the figure.
        assert!(
            store.kept_bytes() >= old,
            "the old files dropped out at the start: {}",
            store.kept_bytes()
        );
        store.settled().await;
        assert!(only(&store).ready(), "{:?}", only(&store).state);
        assert!(
            store.kept_bytes() >= disk_bytes(&dir).bytes,
            "the folder's bytes are not counted"
        );
        assert!(!store.size_unknown(), "the folder stayed unknown");
        // Another item is admitted.
        store.start(request("2")).unwrap();
        store.settled().await;
        FAIL_REMOVE.with(|f| f.set(false));
        assert_eq!(store.videos("u1").len(), 2);
    }

    /// A download restarted between a sweep's choosing a folder and its
    /// deleting it is refused, so the new files are not deleted from under it.
    #[tokio::test]
    async fn a_restart_between_a_sweeps_pick_and_its_delete_is_refused() {
        let rig = Rig::new("sweep-restart");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        FAIL_REMOVE.with(|f| f.set(false));
        assert!(store.kept_bytes() > 0, "no tombstone to sweep");
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (s2, seen2, entered) = (store.clone(), seen.clone(), std::cell::Cell::new(false));
        DURING_REMOVE.with(|d| {
            *d.borrow_mut() = Some(Box::new(move || {
                if !entered.replace(true) {
                    *seen2.borrow_mut() = Some(s2.start(request("1")));
                }
            }));
        });
        store.inner.sweep_tombstones();
        DURING_REMOVE.with(|d| *d.borrow_mut() = None);
        let during = seen.take().expect("the sweep did not delete");
        assert!(
            matches!(during, Err(KeepVideoRefusal::Host(_))),
            "a restart in the gap was admitted: {during:?}"
        );
        assert!(!rig.dir.join("u1").join("1").exists(), "the folder stayed");
        // Once the sweep is over the item starts again.
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(only(&store).ready());
    }

    /// Parks a download's SEG0 body mid-way, with its staging file open.
    async fn park_writer(rig: &Rig, store: &Arc<DiskVideoStore>) -> Arc<AtomicBool> {
        let release = rig.server.hold_mid_body(SEG0);
        store.start(request("1")).unwrap();
        let mut waited = 0;
        while store.staged_bytes() == 0 {
            waited += 1;
            assert!(waited < 2_000, "the write never started");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        release
    }

    /// What the revoked writer's end does to the books, done by hand where a
    /// test cannot let the task run: it unregisters (moving the folder's
    /// epoch) and the run's end sweeps.
    fn end_writer(store: &Arc<DiskVideoStore>) {
        {
            let mut st = lock(&store.inner.state);
            let dir = st.dir_of("u1", "1");
            st.writers.clear();
            st.bump(&dir);
        }
        store.inner.sweep_tombstones();
    }

    /// A removal whose manifest will not unlink puts the record back; a
    /// revoked run's writer still in its folder keeps the folder unknown
    /// (its staging file is not counted) until the writer has stopped.
    #[tokio::test]
    async fn a_restored_record_with_a_revoked_writer_running_stays_unknown() {
        let rig = Rig::new("restore-writer");
        let store = rig.open();
        let release = park_writer(&rig, &store).await;
        let dir = rig.dir.join("u1").join("1");
        std::fs::remove_file(dir.join("manifest.json")).ok();
        std::fs::create_dir(dir.join("manifest.json")).unwrap();
        std::fs::write(dir.join("manifest.json").join("x"), b"x").unwrap();
        store.remove("u1", "1");
        assert_eq!(store.videos("u1").len(), 1, "the record did not come back");
        assert!(
            store.size_unknown(),
            "the mark cleared with a writer still in the folder"
        );
        release.store(true, Ordering::SeqCst);
        let mut waited = 0;
        while store.size_unknown() {
            waited += 1;
            assert!(waited < 2_000, "still unknown after the writer stopped");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let parts = std::fs::read_dir(dir.join("files"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(parts, 0, "the revoked run's staging file was left");
    }

    /// The writer stops while the removal is still deleting (the sweep its
    /// run's end makes finds the folder in removal and leaves it): the
    /// removal measures at its end, so the mark does not stick.
    #[tokio::test]
    async fn a_writer_that_stops_during_the_removal_does_not_leave_the_folder_unknown() {
        let rig = Rig::new("writer-during-removal");
        let store = rig.open();
        let release = park_writer(&rig, &store).await;
        let (s2, entered) = (store.clone(), std::cell::Cell::new(false));
        DURING_REMOVE.with(|d| {
            *d.borrow_mut() = Some(Box::new(move || {
                if !entered.replace(true) {
                    end_writer(&s2);
                }
            }));
        });
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        FAIL_REMOVE.with(|f| f.set(false));
        DURING_REMOVE.with(|d| *d.borrow_mut() = None);
        assert!(
            !store.size_unknown(),
            "the mark stuck after the writer stopped mid-removal"
        );
        release.store(true, Ordering::SeqCst);
    }

    /// The writer stops just after the removal has left its folder and
    /// before it is measured: whichever of the two measures last, the folder
    /// ends up known.
    #[tokio::test]
    async fn a_writer_that_stops_between_the_removal_and_its_measurement_is_measured() {
        let rig = Rig::new("writer-after-removal");
        let store = rig.open();
        let release = park_writer(&rig, &store).await;
        let (s2, entered) = (store.clone(), std::cell::Cell::new(false));
        AFTER_UNMARKED.with(|d| {
            *d.borrow_mut() = Some(Box::new(move || {
                if !entered.replace(true) {
                    end_writer(&s2);
                }
            }));
        });
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        FAIL_REMOVE.with(|f| f.set(false));
        AFTER_UNMARKED.with(|d| *d.borrow_mut() = None);
        assert!(!store.size_unknown(), "the mark stuck");
        release.store(true, Ordering::SeqCst);
    }

    /// A running download put back after a failed unlink becomes a Failed
    /// record, whose bytes are its estimate: it must not reserve less than
    /// it did when it had written more than it planned.
    #[tokio::test]
    async fn a_restored_download_keeps_what_it_reserved() {
        let rig = Rig::new("unlink-fails-reserve");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let big = 50 * GIB;
        {
            let mut st = lock(&store.inner.state);
            let v = st.list.first_mut().unwrap();
            v.state = KeptState::Downloading {
                done: 0,
                total: 1,
                bytes: big,
            };
        }
        assert_eq!(store.kept_bytes(), big);
        let dir = rig.dir.join("u1").join("1");
        std::fs::remove_file(dir.join("manifest.json")).unwrap();
        std::fs::create_dir(dir.join("manifest.json")).unwrap();
        std::fs::write(dir.join("manifest.json").join("x"), b"x").unwrap();
        store.remove("u1", "1");
        assert!(matches!(only(&store).state, KeptState::Failed { .. }));
        assert_eq!(store.kept_bytes(), big, "the reservation shrank");
    }

    /// While a removed download's folder is being deleted its bytes still
    /// count: the record is gone from the list but not from the cap.
    #[tokio::test]
    async fn a_folder_being_deleted_still_counts_against_the_cap() {
        let rig = Rig::new("count-while-removing");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let kept = store.kept_bytes();
        assert!(kept > 0);
        let seen = std::rc::Rc::new(std::cell::Cell::new(u64::MAX));
        let (s2, seen2) = (store.clone(), seen.clone());
        DURING_REMOVE
            .with(|d| *d.borrow_mut() = Some(Box::new(move || seen2.set(s2.kept_bytes()))));
        store.remove("u1", "1");
        DURING_REMOVE.with(|d| *d.borrow_mut() = None);
        assert_eq!(seen.get(), kept, "counted for nothing mid-delete");
        assert_eq!(store.kept_bytes(), 0, "and for nothing once it is gone");
    }

    /// A folder that will not delete keeps counting against the cap, does not
    /// bring the download back, and is deleted by a later sweep.
    #[tokio::test]
    async fn a_folder_that_will_not_delete_stays_counted_until_a_sweep_deletes_it() {
        let rig = Rig::new("tombstone");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        // What stays once the record's manifest is gone: the files.
        let left = dir_size(&dir.join("files")).bytes;
        assert!(left > 0);

        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        assert!(store.videos("u1").is_empty());
        assert!(dir.exists(), "the folder would not go");
        assert_eq!(store.kept_bytes(), left, "its bytes still count");
        assert!(
            lines("pharos: video keep remove")[0].contains("kept_on_disk bytes="),
            "{:?}",
            lines("pharos: video keep remove")
        );

        // Not back after a restart, and still counted while it will not go.
        drop(store);
        let store = rig.open();
        assert!(
            store.videos("u1").is_empty(),
            "the removed download is back"
        );
        assert_eq!(store.kept_bytes(), left);

        // Once the folder can go, the next start sweeps it.
        FAIL_REMOVE.with(|f| f.set(false));
        store.start(request("2")).unwrap();
        store.settled().await;
        assert!(!dir.exists(), "the sweep deleted it");
        let kept = store.videos("u1")[0].bytes();
        assert_eq!(store.kept_bytes(), kept, "only the new download counts");
        assert_eq!(lines("pharos: video keep tombstone cleared").len(), 1);
    }

    /// A restart finds a removed download's folder without its record and
    /// deletes it when it can.
    #[tokio::test]
    async fn a_restart_deletes_a_removed_downloads_leftover_folder() {
        let rig = Rig::new("tombstone-open");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let dir = rig.dir.join("u1").join("1");
        FAIL_REMOVE.with(|f| f.set(true));
        store.remove("u1", "1");
        FAIL_REMOVE.with(|f| f.set(false));
        drop(store);
        assert!(dir.exists());
        let store = rig.open();
        assert!(!dir.exists(), "the open deleted it");
        assert_eq!(store.kept_bytes(), 0);
    }

    #[tokio::test]
    async fn a_download_removed_while_running_stops_without_failing() {
        let rig = Rig::new("gone");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.remove("u1", "1");
        store.settled().await;
        assert!(store.videos("u1").is_empty());
        assert!(lines("pharos: video keep failed").is_empty());
        assert!(!rig.dir.join("u1").join("1").join("manifest.json").exists());
    }

    /// A download that was removed and started again has a new run; the
    /// old run's late failure must not overwrite the new one's record.
    #[tokio::test]
    async fn a_removed_runs_late_failure_does_not_touch_the_restarted_download() {
        let rig = Rig::new("stale-failure");
        let release = rig.server.hold_first(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        rig.server.asked_times(SEG0, 1).await;
        store.remove("u1", "1");
        store.start(request("1")).unwrap();
        rig.server.asked_times(SEG0, 2).await;
        // Let the new run finish, then the old run fail.
        while only(&store).state != KeptState::Ready {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        release.store(true, Ordering::SeqCst);
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
        assert!(lines("pharos: video keep failed").is_empty());
    }

    /// Two runs of one item never share a staging file, and a run that
    /// lost the item cannot put its bytes in the finished file: the old
    /// run reaches its fetch only after the new one has begun staging.
    #[tokio::test]
    async fn a_removed_run_cannot_write_into_the_restarted_downloads_file() {
        let rig = Rig::new("stale-part");
        let old_open = rig.server.slow_first_open(SEG0);
        let store = rig.open();
        store.start(request("1")).unwrap();
        rig.server.asked_times(SEG0, 1).await;
        store.remove("u1", "1");
        // The new run's body for SEG0 stops half way, staging in the file.
        let new_body = rig.server.hold_mid_body(SEG0);
        store.start(request("1")).unwrap();
        rig.server.asked_times(SEG0, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // The old run now gets a different answer and stages it too.
        rig.server
            .files
            .lock()
            .unwrap()
            .insert(SEG0.into(), vec![7; 100]);
        old_open.store(true, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        rig.server
            .files
            .lock()
            .unwrap()
            .insert(SEG0.into(), vec![2; 100]);
        new_body.store(true, Ordering::SeqCst);
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
        let file = store.stored_file("u1", SEG0).unwrap();
        let bytes = std::fs::read(file.path).unwrap();
        assert!(
            bytes.len() == 100 && bytes.iter().all(|b| *b != 7),
            "the old run's bytes are not in the file: {bytes:?}"
        );
        let dir = rig.dir.join("u1").join("1").join("files");
        assert!(
            std::fs::read_dir(dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")),
            "no staging file is left"
        );
    }

    /// Position saves racing a removal must not bring the folder back, and
    /// racing each other must leave one whole manifest and no temp file.
    #[tokio::test]
    async fn saves_racing_a_removal_do_not_recreate_the_item() {
        let rig = Rig::new("save-race");
        let store = rig.open();
        for round in 0..25u64 {
            store.start(request("1")).unwrap();
            store.settled().await;
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let spammers: Vec<_> = (0..2)
                .map(|_| {
                    let (store, stop) = (store.clone(), stop.clone());
                    std::thread::spawn(move || {
                        let mut at = 0;
                        while !stop.load(Ordering::SeqCst) {
                            at += 1;
                            store.save_position("u1", "1", at);
                        }
                    })
                })
                .collect();
            std::thread::sleep(std::time::Duration::from_millis(2));
            let item_dir = rig.dir.join("u1").join("1");
            let bytes = std::fs::read(item_dir.join("manifest.json")).unwrap();
            assert!(
                serde_json::from_slice::<KeptVideo>(&bytes).is_ok(),
                "round {round}: the manifest is whole while saves race"
            );
            store.remove("u1", "1");
            stop.store(true, Ordering::SeqCst);
            for t in spammers {
                t.join().unwrap();
            }
            assert!(
                !item_dir.exists(),
                "round {round}: a late save brought the removed item back"
            );
        }
    }

    #[tokio::test]
    async fn downloads_are_listed_per_user_and_remember_a_position() {
        let rig = Rig::new("users");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert!(store.videos("u2").is_empty());
        store.save_position("u1", "1", 61_000);
        drop(store);
        assert_eq!(only(&rig.open()).position_ms, 61_000);
    }

    #[tokio::test]
    async fn the_short_last_segment_may_be_skipped() {
        let rig = Rig::new("tail");
        let media = MEDIA.replace("#EXTINF:6.0,\n1.m4s", "#EXTINF:0.042,\n1.m4s");
        rig.server
            .files
            .lock()
            .unwrap()
            .insert(MEDIA_KEY.into(), media.into_bytes());
        rig.server.fail(SEG1, &[403]);
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
        assert!(store.stored_file("u1", SEG1).is_none());
        assert_eq!(lines("pharos: video keep skipped short tail").len(), 1);
        assert!(!only(&store).files.contains(&SEG1.to_owned()));

        // A reopen leaves it Ready: the skipped file is not "missing".
        drop(store);
        rig.server.asked.lock().unwrap().clear();
        let store = rig.open();
        assert!(
            lines("pharos: video keep resume").is_empty(),
            "{:?}",
            lines("pharos: video keep resume")
        );
        store.settled().await;
        assert_eq!(only(&store).state, KeptState::Ready);
        assert!(rig.server.asked().is_empty(), "{:?}", rig.server.asked());
    }

    /// The proxy asks with the path and query the WebView sent; a stored
    /// file is found by that, and a different play session is a miss.
    #[tokio::test]
    async fn a_proxy_request_hits_by_path_and_query_and_misses_otherwise() {
        let rig = Rig::new("lookup");
        let store = rig.open();
        store.start(request("1")).unwrap();
        store.settled().await;
        let hit = |url: &str| store.stored_file("u1", &request_key(url));
        assert!(hit("/videos/1/h264cmaf/0.m4s?PlaySessionId=p").is_some());
        assert!(hit("http://localhost:8096/videos/1/h264cmaf/0.m4s?PlaySessionId=p").is_some());
        assert!(hit("/videos/1/h264cmaf/0.m4s?PlaySessionId=other").is_none());
        assert!(hit("/videos/1/h264cmaf/0.m4s").is_none());
        assert!(hit("/videos/1/h264cmaf/9.m4s?PlaySessionId=p").is_none());
        assert!(hit("/Items/1/Images/Primary").is_none());
    }

    #[test]
    fn names_are_safe_and_stable() {
        assert_eq!(safe_name("ab-12_Z"), "ab-12_Z");
        assert_eq!(safe_name("../x"), "%2e%2e%2fx");
        assert_eq!(safe_name(""), "_");
        assert_eq!(file_name("/a?b"), file_name("/a?b"));
        assert_ne!(file_name("/a?b"), file_name("/a?c"));
        assert_eq!(file_name("/a").len(), 32);
    }

    #[test]
    fn the_clock_reads_as_iso_8601_utc() {
        assert_eq!(iso_from_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_secs(1_784_192_523), "2026-07-16T09:02:03Z");
        assert_eq!(iso_from_secs(951_782_400), "2000-02-29T00:00:00Z");
    }
}
