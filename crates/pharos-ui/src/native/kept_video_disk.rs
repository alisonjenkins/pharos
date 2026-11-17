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
    index: HashMap<String, (PathBuf, u64)>,
    /// The run that owns each (user, item) download; a removed or restarted
    /// download is told apart from the one still running by this number.
    runs: HashMap<(String, String), u64>,
    next_run: u64,
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

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
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
    /// the same lock while it deletes the folder.
    fn save_record(&self, user: &str, item: &str) {
        let _saving = self.saves.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(video) = find(&lock(&self.state).list, user, item).cloned() else {
            return;
        };
        let dir = self.item_dir(&video.user, &video.item);
        let result = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let text = serde_json::to_vec(&video).map_err(std::io::Error::other)?;
            let tmp = dir.join("manifest.json.part");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, dir.join("manifest.json"))
        })();
        if let Err(e) = result {
            emit(
                true,
                format!(
                    "pharos: video record not saved item={} dir={} error={e}",
                    video.item,
                    dir.display()
                ),
            );
        }
    }

    /// Changes the record of `item` and saves it; false when it is gone.
    fn update(&self, user: &str, item: &str, f: impl FnOnce(&mut KeptVideo)) -> bool {
        {
            let mut st = lock(&self.state);
            let Some(v) = st
                .list
                .iter_mut()
                .find(|v| v.user == user && v.item == item)
            else {
                return false;
            };
            f(v);
        }
        self.save_record(user, item);
        true
    }

    /// [`Inner::update`] for the run that owns the download: the run id is
    /// checked under the same lock as the change, so a run that was removed
    /// or restarted since cannot touch the record of the one that replaced
    /// it. False when `run` no longer owns `item`.
    fn update_run(&self, user: &str, item: &str, run: u64, f: impl FnOnce(&mut KeptVideo)) -> bool {
        {
            let mut st = lock(&self.state);
            if st.runs.get(&(user.to_owned(), item.to_owned())) != Some(&run) {
                return false;
            }
            let Some(v) = st
                .list
                .iter_mut()
                .find(|v| v.user == user && v.item == item)
            else {
                return false;
            };
            f(v);
        }
        self.save_record(user, item);
        true
    }

    fn wanted(&self, user: &str, item: &str, run: u64) -> bool {
        let st = lock(&self.state);
        st.runs.get(&(user.to_owned(), item.to_owned())) == Some(&run)
            && find(&st.list, user, item)
                .is_some_and(|v| matches!(v.state, KeptState::Downloading { .. }))
    }

    /// Every video byte on the device, leaving out `except`'s record.
    fn video_used(&self, except: Option<(&str, &str)>) -> u64 {
        lock(&self.state)
            .list
            .iter()
            .filter(|v| except != Some((v.user.as_str(), v.item.as_str())))
            .map(KeptVideo::bytes)
            .sum()
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
        st.index.insert(key.to_owned(), (path, len));
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
            self.update_run(user, item, run, |v| {
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
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| store_err("create the folder for", e))?;
    }
    let part = path.with_extension(format!("{run}.part"));
    let mut file = tokio::fs::File::create(&part)
        .await
        .map_err(|e| store_err("create", e))?;
    let mut len = 0u64;
    loop {
        match body.next_chunk().await {
            Ok(Some(chunk)) => {
                file.write_all(&chunk)
                    .await
                    .map_err(|e| store_err("write", e))?;
                len = len.saturating_add(chunk.len() as u64);
            }
            Ok(None) => break,
            Err(e) => {
                drop(file);
                let _ = tokio::fs::remove_file(&part).await;
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
            let _ = tokio::fs::remove_file(&part).await;
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
    let Ok(read) = std::fs::read_dir(dir.join("files")) else {
        return HashMap::new();
    };
    read.filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".part") {
                return None;
            }
            Some((name, e.metadata().ok()?.len()))
        })
        .collect()
}

/// Deletes the staging files a closed app left in `dir/files`.
fn clear_parts(dir: &Path) {
    let Ok(read) = std::fs::read_dir(dir.join("files")) else {
        return;
    };
    for entry in read.filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().ends_with(".part") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Deletes the stored files of `item` that `plan` does not name, and
/// forgets them: a retry that planned afresh (a new play session names
/// every file anew) must not leave the old plan's files on the disk.
fn prune_unplanned(inner: &Inner, user: &str, item: &str, plan: &[String]) {
    let dir = inner.item_dir(user, item);
    let keep: HashSet<String> = plan.iter().map(|k| file_name(k)).collect();
    let files = dir.join("files");
    let (mut removed, mut bytes) = (0usize, 0u64);
    for (name, len) in stored_names(&dir) {
        if !keep.contains(&name) && std::fs::remove_file(files.join(&name)).is_ok() {
            removed += 1;
            bytes = bytes.saturating_add(len);
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
    if !resume {
        let free = inner.env.budget.free_bytes();
        let used = inner
            .video_used(Some((user, item)))
            .saturating_add(inner.env.budget.other_used_bytes());
        let cap = inner.env.budget.cap_bytes();
        if let Err(refusal) = fits(estimate, used, cap, free) {
            emit(
                true,
                format!(
                    "pharos: video keep refused item={item} reason={refusal} estimate={estimate} used={used} cap={cap} free={free}"
                ),
            );
            for key in &stored {
                let _ = tokio::fs::remove_file(inner.file_path(user, item, key)).await;
                lock(&inner.state).index.remove(key);
            }
            return Err(Stop::Refused(refusal.to_string()));
        }
    }
    let total = u32::try_from(all.len()).unwrap_or(u32::MAX);
    let planned = all.clone();
    if !inner.update_run(user, item, run, |v| {
        v.files = planned;
        v.estimated_bytes = estimate;
        v.state = KeptState::Downloading {
            done: 0,
            total,
            bytes: 0,
        };
    }) {
        return Err(Stop::Gone);
    }
    prune_unplanned(inner, user, item, &all);

    let names = stored_names(&inner.item_dir(user, item));
    let mut have = HashSet::new();
    let mut bytes = 0u64;
    for key in &all {
        if let Some(len) = names.get(&file_name(key)) {
            have.insert(key.clone());
            bytes = bytes.saturating_add(*len);
            inner.record_file(user, item, run, key, inner.file_path(user, item, key), *len);
        }
    }
    if resume {
        // The cap was met when the download began and its own bytes are
        // already counted; what is still to come needs room, and an
        // unreadable measure (0) is no room.
        let free = inner.env.budget.free_bytes();
        let remaining = estimate.saturating_sub(bytes);
        if let Err(refusal) = fits(remaining, 0, u64::MAX, free) {
            emit(
                true,
                format!(
                    "pharos: video keep refused item={item} reason={refusal} resume=true remaining={remaining} stored={bytes} free={free}"
                ),
            );
            return Err(Stop::Refused(refusal.to_string()));
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
    match outcome {
        Ok(Finished {
            bytes,
            files,
            skipped,
        }) => {
            if inner.update_run(&user, &item, run, |v| {
                // A skipped file is not on disk: left in the record, a
                // reopen would count it missing and resume the copy.
                v.files.retain(|k| !skipped.contains(k));
                v.state = KeptState::Ready;
                v.estimated_bytes = bytes;
            }) {
                emit(
                    false,
                    format!(
                        "pharos: video keep ready item={item} bytes={bytes} files={files} ms={ms}"
                    ),
                );
            } else {
                superseded("superseded");
            }
        }
        Err(Stop::Gone) => superseded("removed"),
        Err(Stop::Refused(reason) | Stop::Failed(reason)) => {
            let (mut files, mut bytes) = (0, 0);
            let applied = inner.update_run(&user, &item, run, |v| {
                if let KeptState::Downloading { done, .. } = v.state {
                    files = done;
                }
                bytes = v.bytes();
                v.state = KeptState::Failed {
                    reason: reason.clone(),
                };
            });
            if applied {
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
    drop_parts_of(&inner.item_dir(&user, &item), run);
    let mut st = lock(&inner.state);
    if st.runs.get(&(user.clone(), item.clone())) == Some(&run) {
        st.runs.remove(&(user, item));
    }
}

/// Deletes the staging files `run` left in `dir/files`.
fn drop_parts_of(dir: &Path, run: u64) {
    let Ok(read) = std::fs::read_dir(dir.join("files")) else {
        return;
    };
    let suffix = format!(".{run}.part");
    for entry in read.filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().ends_with(&suffix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
        for user_dir in dirs(&inner.root) {
            for item_dir in dirs(&user_dir) {
                let manifest = item_dir.join("manifest.json");
                let record = match std::fs::read(&manifest) {
                    Ok(bytes) => match serde_json::from_slice::<KeptVideo>(&bytes) {
                        Ok(v) => v,
                        Err(e) => {
                            emit(
                                true,
                                format!(
                                    "pharos: video record damaged dir={} error={e}",
                                    item_dir.display()
                                ),
                            );
                            continue;
                        }
                    },
                    Err(_) => continue,
                };
                clear_parts(&item_dir);
                let names = stored_names(&item_dir);
                let mut whole = 0usize;
                for key in &record.files {
                    if let Some(len) = names.get(&file_name(key)) {
                        whole += 1;
                        index.insert(
                            key.clone(),
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

fn dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
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
        // planning and shows as a failed download.
        let used = self
            .inner
            .video_used(Some((&user, &item)))
            .saturating_add(budget.other_used_bytes());
        let early = fits(0, used, budget.cap_bytes(), budget.free_bytes());
        let started = early.and_then(|()| {
            let mut st = lock(&self.inner.state);
            check_not_kept(&st.list, &user, &item)?;
            insert_front(&mut st.list, new_record(&request, (self.inner.env.now)()));
            Ok(())
        });
        if let Err(refusal) = started {
            emit(
                true,
                format!("pharos: video keep refused item={item} reason={refusal} used={used}"),
            );
            return Err(refusal);
        }
        self.inner.save_record(&user, &item);
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
                st.index.remove(key);
            }
            record
        };
        let dir = self.inner.item_dir(user, item);
        let removed = std::fs::remove_dir_all(&dir);
        emit(
            false,
            format!(
                "pharos: video keep remove item={item} files={} verdict={}",
                record.files.len(),
                match &removed {
                    Ok(()) => "removed".to_owned(),
                    Err(e) => format!("kept_on_disk error={e}"),
                }
            ),
        );
    }

    fn save_position(&self, user: &str, item: &str, position_ms: u64) {
        self.inner
            .update(user, item, |v| v.position_ms = position_ms);
    }

    fn cap_bytes(&self) -> u64 {
        self.inner.env.budget.cap_bytes()
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

    fn stored_file(&self, key: &str) -> Option<StoredFile> {
        let (path, len) = lock(&self.inner.state).index.get(key).cloned()?;
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
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::*;
    use crate::native::kept_video::DEFAULT_QUALITY_BPS;
    use crate::native::offline_ui::{FREE_GUARD_BYTES, GIB};

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
        free: AtomicU64,
    }

    impl FakeBudget {
        fn roomy() -> Arc<Self> {
            Arc::new(Self {
                cap: AtomicU64::new(4 * GIB),
                other: AtomicU64::new(0),
                free: AtomicU64::new(100 * GIB),
            })
        }
    }

    impl Budget for FakeBudget {
        fn cap_bytes(&self) -> u64 {
            self.cap.load(Ordering::SeqCst)
        }
        fn other_used_bytes(&self) -> u64 {
            self.other.load(Ordering::SeqCst)
        }
        fn free_bytes(&self) -> u64 {
            self.free.load(Ordering::SeqCst)
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
            self.opened.fetch_add(1, Ordering::SeqCst);
            let slept = self.slept.clone();
            DiskVideoStore::open(
                self.dir.clone(),
                Env {
                    fetcher: self.server.clone(),
                    budget: self.budget.clone(),
                    sleep: Arc::new(move |secs| {
                        slept.lock().unwrap().push(secs);
                        Box::pin(async {})
                    }),
                    now: || "2026-11-16T09:00:00Z".to_owned(),
                    runtime: tokio::runtime::Handle::current(),
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
            let f = store.stored_file(key).unwrap();
            assert_eq!(std::fs::read(&f.path).unwrap().first(), Some(&byte));
            assert_eq!(f.len, std::fs::metadata(&f.path).unwrap().len());
        }
        assert_eq!(store.stored_file(SEG0).unwrap().content_type, "video/mp4");
        assert_eq!(
            store.stored_file(MEDIA_KEY).unwrap().content_type,
            "application/vnd.apple.mpegurl"
        );
        assert!(store.stored_file("/videos/1/other.m4s").is_none());
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
        let gone = store.stored_file(SEG1).unwrap().path;
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
        assert!(store.stored_file(SEG1).is_some());
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
        assert!(store.stored_file(MASTER_KEY).is_none());
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
            let gone = store.stored_file(SEG1).unwrap().path;
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
            assert!(store.stored_file(SEG0).is_some());
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
        assert!(store.stored_file(INIT).is_some(), "the old plan left files");

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
            store.stored_file(INIT).is_none(),
            "the old file is not indexed"
        );
        let dir = rig.dir.join("u1").join("1").join("files");
        let on_disk = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(on_disk, v.files.len(), "only the new plan's files remain");
        assert_eq!(v.files.len(), 5);
        assert_eq!(lines("pharos: video keep pruned").len(), 1);
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
        assert!(store.stored_file(SEG0).is_none());
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
        let file = store.stored_file(SEG0).unwrap();
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
        assert!(store.stored_file(SEG1).is_none());
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
        let hit = |url: &str| store.stored_file(&request_key(url));
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
