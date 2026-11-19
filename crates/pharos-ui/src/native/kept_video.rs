//! Videos downloaded for watching with no signal (spec 048, ADR 0143).
//!
//! Shared by every app that plays video: the web app stores the bytes in
//! Cache Storage (`kept_video_web.rs`), the phone app under its files dir.
//! Both sit behind the [`VideoStore`] port, so the Download control, the
//! Downloads page and the offline player draw the same on each.
//!
//! A download is the player's own HLS stream at one fixed quality: every
//! playlist, init and segment response stored under the exact URL the
//! player requests (path and query). Played back, the same URLs are asked
//! for and the store answers them, so the player code does not change.
//! `pharos_hls::offline` lists the URLs and estimates the size.

use serde::{Deserialize, Serialize};

use super::offline_ui::{FREE_GUARD_BYTES, GIB};

/// Every downloaded video together stays under this unless the viewer
/// raises it (the same default as offline music on the phone).
pub const DEFAULT_CAP_BYTES: u64 = 4 * GIB;

/// The caps a viewer can pick for downloaded video, in GiB. Wider than
/// offline music's: a film at 2 Mbps is about 1.5 GiB.
pub const CAP_CHOICES_GIB: [u64; 7] = [2, 4, 8, 16, 32, 64, 128];

/// Where a device keeps the viewer's chosen cap (the GiB count as text).
pub const CAP_KEY: &str = "pharos.kept_videos.cap_gib";

/// The cap choices as `Choices` options: the GiB count and its label.
pub fn cap_options() -> Vec<(String, String)> {
    CAP_CHOICES_GIB
        .iter()
        .map(|g| (g.to_string(), format!("{g} GiB")))
        .collect()
}

/// The option a cap presses; empty for a cap that is not a choice.
pub fn cap_selected(cap_bytes: u64) -> String {
    CAP_CHOICES_GIB
        .iter()
        .find(|g| g.saturating_mul(GIB) == cap_bytes)
        .map(u64::to_string)
        .unwrap_or_default()
}

/// The bytes an option stands for; `None` for anything not offered.
pub fn cap_from_option(value: &str) -> Option<u64> {
    let gib: u64 = value.trim().parse().ok()?;
    CAP_CHOICES_GIB
        .contains(&gib)
        .then(|| gib.saturating_mul(GIB))
}

/// The cap a stored value gives: the choice it names, else the default
/// (nothing stored, or a value from a build with other choices).
pub fn cap_from_stored(stored: Option<&str>) -> u64 {
    stored
        .and_then(cap_from_option)
        .unwrap_or(DEFAULT_CAP_BYTES)
}

/// The qualities a download can be made at, as the video bitrate cap the
/// player negotiates with (bits per second), lowest first.
pub const QUALITY_CHOICES_BPS: [u64; 3] = [1_000_000, 2_000_000, 4_000_000];

/// The quality picked when the viewer has not chosen: about 0.9 GiB an hour.
pub const DEFAULT_QUALITY_BPS: u64 = 2_000_000;

/// A quality's name on the Download control.
pub fn quality_label(bps: u64) -> &'static str {
    match bps {
        0..=1_499_999 => "Small (1 Mbps)",
        1_500_000..=2_999_999 => "Medium (2 Mbps)",
        _ => "Large (4 Mbps)",
    }
}

/// One subtitle track kept beside the video, as WebVTT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptSubtitle {
    /// The stream index the player names the track by.
    pub index: u32,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub language: Option<String>,
    /// The exact URL the player fetches it from; stored under that key.
    pub url: String,
}

/// Where a download is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum KeptState {
    /// Fetching: `done` of `total` files stored, `bytes` so far.
    Downloading { done: u32, total: u32, bytes: u64 },
    /// Every file stored: plays with no signal.
    Ready,
    /// Stopped short; `reason` names the cause and the offending value.
    Failed { reason: String },
}

/// One video on this device: what the Downloads page and the offline
/// player need, with no call to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptVideo {
    // Identity and the stored files have no default: a record without them
    // names nothing it could serve. Every other field falls back, so a field
    // added later does not strand the manifests written before it (a
    // stranded manifest holds its folder, and the admission block with it).
    /// The item, as `ItemRef` prints it.
    pub item: String,
    /// The version played; empty is the primary.
    #[serde(default)]
    pub version: String,
    /// The signed-in user's id (hex, as `Session.me` reports), so one
    /// user's downloads are never listed for another.
    pub user: String,
    /// The display name ("Show · S1E2 · Title" for an episode).
    #[serde(default)]
    pub title: String,
    /// "movie" or "episode".
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub runtime_ms: u64,
    /// The exact master playlist URL the player opens.
    pub master_url: String,
    /// The audio and subtitle stream indexes the stream was made with.
    #[serde(default)]
    pub audio: Option<u32>,
    #[serde(default)]
    pub subtitles: Vec<KeptSubtitle>,
    /// Every stored key, master first.
    pub files: Vec<String>,
    /// The video bitrate cap it was made at.
    #[serde(default)]
    pub quality_bps: u64,
    /// The size estimated before starting.
    #[serde(default)]
    pub estimated_bytes: u64,
    pub state: KeptState,
    /// Where the viewer stopped, saved on the device while offline.
    #[serde(default)]
    pub position_ms: u64,
    /// ISO 8601 UTC.
    #[serde(default)]
    pub saved_at: String,
    /// The estimate was not set by an admitted plan: the record was adopted
    /// from a manifest that lacked one, so it is the folder's measured size
    /// and the files still missing were never reserved. A resume of such a
    /// record plans again and passes the shared-cap check as a fresh
    /// download. True when the manifest has no such field; false once a plan
    /// has been admitted and saved.
    #[serde(default = "estimate_from_adoption_default")]
    pub estimate_from_adoption: bool,
    /// The store's accounting, never saved: the bytes an earlier plan left
    /// on disk that this record's admitted plan could not prune. Part of
    /// its reservation and on disk already, so not owed. It lives and dies
    /// with the record: a removal or a replacing record drops it, a
    /// rollback that puts the record back restores it, and no other site
    /// has to clear it.
    #[serde(skip)]
    pub stale_on_disk: u64,
}

fn estimate_from_adoption_default() -> bool {
    true
}

impl KeptVideo {
    pub fn ready(&self) -> bool {
        self.state == KeptState::Ready
    }

    /// Bytes this download holds now.
    pub fn bytes(&self) -> u64 {
        match &self.state {
            KeptState::Downloading { bytes, .. } => *bytes,
            KeptState::Ready | KeptState::Failed { .. } => self.estimated_bytes,
        }
    }
}

/// What the Download control hands the store to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepVideoRequest {
    pub item: String,
    pub version: String,
    pub user: String,
    pub title: String,
    pub kind: String,
    pub runtime_ms: u64,
    /// `Decision.stream_url` from a negotiation that forced HLS at
    /// `quality_bps`.
    pub master_url: String,
    pub audio: Option<u32>,
    pub subtitles: Vec<KeptSubtitle>,
    pub quality_bps: u64,
}

/// Why a download was refused before it started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeepVideoRefusal {
    #[error("needs about {needed} bytes but the downloads cap leaves {available}")]
    OverCap { needed: u64, available: u64 },
    #[error("needs about {needed} bytes but only {free} are free beyond the {guard}-byte guard")]
    LowSpace { needed: u64, free: u64, guard: u64 },
    #[error("already on this device: {item}")]
    AlreadyKept { item: String },
    #[error("video store failed: {0}")]
    Host(String),
}

/// Whether a download of `needed` bytes fits: under the cap with what is
/// kept already, and leaving [`FREE_GUARD_BYTES`] free on the device.
pub fn fits(needed: u64, used: u64, cap: u64, free: u64) -> Result<(), KeepVideoRefusal> {
    let available = cap.saturating_sub(used);
    if needed > available {
        return Err(KeepVideoRefusal::OverCap { needed, available });
    }
    if needed.saturating_add(FREE_GUARD_BYTES) > free {
        return Err(KeepVideoRefusal::LowSpace {
            needed,
            free,
            guard: FREE_GUARD_BYTES,
        });
    }
    Ok(())
}

/// The store of downloaded videos. The host fetches and stores in the
/// background; the UI polls [`VideoStore::videos`] for progress.
/// `Send + Sync` so it registers like the other ports; the web store keeps
/// its state in thread-locals, wasm being single-threaded.
pub trait VideoStore: Send + Sync {
    /// The signed-in user's downloads, newest first.
    fn videos(&self, user: &str) -> Vec<KeptVideo>;
    /// Start fetching: plan the files with `pharos_hls::offline`, check
    /// [`fits`], store each under its exact URL, record progress.
    fn start(&self, request: KeepVideoRequest) -> Result<(), KeepVideoRefusal>;
    /// Delete a download's files and record.
    fn remove(&self, user: &str, item: &str);
    /// Note where the viewer stopped, for the next offline start.
    fn save_position(&self, user: &str, item: &str, position_ms: u64);
    fn cap_bytes(&self) -> u64;
    /// What every user's downloads hold or have reserved on this device,
    /// for the host to count against the cap it shares with other kinds.
    /// Zero for a store that shares no cap.
    fn kept_bytes(&self) -> u64 {
        0
    }
    /// What every user's running downloads are admitted for but have yet
    /// to write, for the host to take off the free space it guards for
    /// other kinds. Zero for a store that shares no disk.
    fn outstanding_bytes(&self) -> u64 {
        0
    }
    /// Whether the store holds a folder whose size or reservation it could
    /// not read, so the host cannot trust the cap it shares: nothing else
    /// is admitted either. False for a store that shares no cap.
    fn size_unknown(&self) -> bool {
        false
    }
    /// The three figures the music cache reads, as one snapshot: what is
    /// held, what is still owed, and whether a size is unknown. A store
    /// with state of its own reads them in one hold, so the verdict and the
    /// bytes describe the same moment.
    fn accounting(&self) -> (u64, u64, bool) {
        let unknown = self.size_unknown();
        (self.kept_bytes(), self.outstanding_bytes(), unknown)
    }
    /// How many folders hold downloads whose record cannot be read: they
    /// are kept, counted and block every admission until they read again or
    /// are discarded. Zero for a store with no such folders.
    fn unreadable_count(&self) -> usize {
        0
    }
    /// Deletes the folders whose record still cannot be read, returning the
    /// bytes freed. Err names why it could not, or what stayed. A store with
    /// no folders to discard (the web's) refuses.
    fn discard_unreadable(&self) -> Result<u64, String> {
        Err("this device keeps no unreadable downloads".to_owned())
    }
    /// Set the cap. A host that cannot change it keeps the default and
    /// says why. Lowering it below what is kept deletes nothing; new
    /// downloads are refused until there is room.
    fn set_cap_bytes(&self, bytes: u64) -> Result<(), String> {
        Err(format!(
            "this device keeps the cap at {DEFAULT_CAP_BYTES} bytes, not {bytes}"
        ))
    }
    fn used_bytes(&self, user: &str) -> u64;
    /// Free space as the host last measured it.
    fn free_bytes(&self) -> u64;
    /// The stored file for a request's [`request_key`] among `user`'s own
    /// downloads, if this store holds it whole: one user never reads
    /// another's. A host that answers the player's requests itself (the
    /// phone's server proxy) asks before going to the network; a store whose
    /// platform answers them another way (the web's service worker) keeps
    /// the default.
    fn stored_file(&self, _user: &str, _key: &str) -> Option<StoredFile> {
        None
    }
    /// A signed-in session ended: every running download of `owner`'s
    /// (every one, with `None`, when whose session it was is not known)
    /// stops before another user's token can be used. Each is parked: its
    /// record and files stay, Downloading, until [`resume_user`]. A store
    /// whose downloads carry no device token keeps the default.
    ///
    /// [`resume_user`]: VideoStore::resume_user
    fn park_runs(&self, _owner: Option<&str>) {}
    /// `user` signed in: their parked downloads run again.
    fn resume_user(&self, _user: &str) {}
}

/// A file a [`VideoStore`] holds, ready to answer a request with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub path: std::path::PathBuf,
    pub len: u64,
    /// The `Content-Type` to answer with: derived from the key's extension
    /// by [`content_type_for`], never stored from the server's header.
    pub content_type: &'static str,
}

/// What follows `scheme://` when `url` starts with one. Only a prefix
/// counts: a `://` inside a path or query (a redirect target, say) is part
/// of that path or query.
fn after_scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(rest)
}

/// The key a request is stored under on a host that sees no scheme or
/// host: the path and the query, which names the play session. An absolute
/// URL is cut down to the same, so both spellings of one file agree. Its
/// authority ends at the first `/`, `?` or `#`: a URL with no path keeps
/// its query, under the path `/`.
pub fn request_key(url: &str) -> String {
    let path_and_query = match after_scheme(url) {
        Some(rest) => rest.find(['/', '?', '#']).map_or("", |i| &rest[i..]),
        None => url,
    };
    let key = path_and_query.split('#').next().unwrap_or_default();
    if key.starts_with('/') {
        key.to_owned()
    } else {
        format!("/{key}")
    }
}

/// The `Content-Type` the server sends for a stored file, by its extension.
pub fn content_type_for(key: &str) -> &'static str {
    let path = url_path(key);
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("m4s" | "mp4" | "m4a" | "m4v") => "video/mp4",
        Some("ts") => "video/mp2t",
        Some("vtt") => "text/vtt",
        _ => "application/octet-stream",
    }
}

/// What a request for a stream is, for the log line a host writes about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    Master,
    Playlist,
    Init,
    Segment,
    Subtitle,
    /// Anything else the page asks the server for: not part of a download.
    Other,
}

impl RequestClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::Playlist => "playlist",
            Self::Init => "init",
            Self::Segment => "segment",
            Self::Subtitle => "subtitle",
            Self::Other => "other",
        }
    }
}

/// The class of the request for `key`.
pub fn request_class(key: &str) -> RequestClass {
    let path = url_path(key);
    let name = path.rsplit('/').next().unwrap_or_default();
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("m3u8") if name.starts_with("master") => RequestClass::Master,
        Some("m3u8") => RequestClass::Playlist,
        Some("mp4") if name.starts_with("init") => RequestClass::Init,
        Some("m4s" | "mp4" | "m4a" | "m4v" | "ts") => RequestClass::Segment,
        Some("vtt") => RequestClass::Subtitle,
        _ => RequestClass::Other,
    }
}

/// Where a host answered a request from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// From a stored file, with no network.
    ServedOffline,
    /// From the server.
    Network,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::ServedOffline => "served_offline",
            Self::Network => "network",
        }
    }
}

/// How loudly a request's verdict is logged: playlists are few and say
/// which copy plays, so they are info; a film has a thousand segments, so
/// those are debug; a request that is no part of a stream is not logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loudness {
    Info,
    Debug,
}

pub fn loudness(class: RequestClass) -> Option<Loudness> {
    match class {
        RequestClass::Master | RequestClass::Playlist => Some(Loudness::Info),
        RequestClass::Init | RequestClass::Segment | RequestClass::Subtitle => {
            Some(Loudness::Debug)
        }
        RequestClass::Other => None,
    }
}

/// The log line for a request's verdict. The path only: the query carries
/// the play session.
pub fn request_line(key: &str, verdict: Verdict, bytes: u64) -> String {
    format!(
        "pharos: video request class={} verdict={} bytes={bytes} url_path={}",
        request_class(key).label(),
        verdict.label(),
        url_path(key)
    )
}

/// Where the record of downloaded videos is kept on a device (a JSON array
/// of [`KeptVideo`]).
pub const RECORDS_KEY: &str = "pharos.kept_videos";

/// Cache Storage names for downloaded video start with this; the service
/// worker looks through every cache with it (ADR 0143).
pub const CACHE_PREFIX: &str = "pharos-videos-";

/// The Cache Storage cache holding `user`'s downloaded video.
pub fn cache_name(user: &str) -> String {
    format!("{CACHE_PREFIX}{user}")
}

/// `url` as an absolute URL against `origin` (`https://host`): the key the
/// service worker's `caches.match(request.url)` finds. An absolute URL is
/// returned as is.
pub fn absolute_url(origin: &str, url: &str) -> String {
    if after_scheme(url).is_some() {
        url.to_owned()
    } else if url.starts_with('/') {
        format!("{}{url}", origin.trim_end_matches('/'))
    } else {
        format!("{}/{url}", origin.trim_end_matches('/'))
    }
}

/// The path of `url`, with no query, for log lines (the query carries the
/// play session).
pub fn url_path(url: &str) -> &str {
    let rest = after_scheme(url).unwrap_or(url);
    let from_path = match after_scheme(url) {
        // The authority ends where `request_key`'s does.
        Some(_) => rest.find(['/', '?', '#']).map_or("", |i| &rest[i..]),
        None => rest.find('/').map_or("", |i| &rest[i..]),
    };
    from_path.split(['?', '#']).next().unwrap_or("")
}

/// Whether a failed `cache.put` is the body transfer breaking (the browser
/// rejects with a `TypeError`) rather than the store refusing it (quota and
/// the like, which a retry cannot mend).
pub fn put_is_retryable(error_name: &str) -> bool {
    matches!(error_name, "TypeError" | "NetworkError")
}

/// Seconds to wait before retry number `attempt` (1-based) of one file, or
/// `None` once the attempts are used up.
pub fn retry_delay_secs(attempt: u32) -> Option<u64> {
    const DELAYS: [u64; 6] = [1, 2, 4, 8, 15, 30];
    DELAYS.get(attempt.checked_sub(1)? as usize).copied()
}

/// The device token a download run for `owner` may send: the stored token,
/// only while the user stored with it is `owner`. `token` and `token_user`
/// come from one read of the one token file, so a sign-in landing between
/// two reads cannot pair one user's token with another user's run. Fails
/// closed: Err names the refusal, `no_token`, `token_user_unknown` (a token
/// whose user is not stored yet, as during a sign-in) or
/// `token_owner_mismatch` with both internal ids.
pub fn token_for_owner(
    token: Option<&str>,
    token_user: Option<&str>,
    owner: &str,
) -> Result<String, String> {
    let Some(token) = token else {
        return Err(format!("no_token owner={owner}"));
    };
    match token_user {
        None => Err(format!("token_user_unknown owner={owner}")),
        Some(user) if user != owner => Err(format!(
            "token_owner_mismatch owner={owner} token_user={user}"
        )),
        Some(_) => Ok(token.to_owned()),
    }
}

/// Whether a failed fetch is worth another try: no answer at all (`None`),
/// a segment not ready yet (404) or a busy server (5xx).
pub fn is_retryable(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(s) => s == 404 || (500..600).contains(&s),
    }
}

/// `files` that `cached` does not hold yet, in order: what a restarted
/// download still has to fetch.
pub fn missing_files(files: &[String], cached: &std::collections::HashSet<String>) -> Vec<String> {
    files
        .iter()
        .filter(|f| !cached.contains(*f))
        .cloned()
        .collect()
}

/// Files of an earlier plan that the new plan no longer names: a retry whose
/// plan changed must not leave them in the cache.
pub fn stale_files(old: &[String], new: &[String]) -> Vec<String> {
    let keep: std::collections::HashSet<&String> = new.iter().collect();
    old.iter().filter(|f| !keep.contains(f)).cloned().collect()
}

/// Whether progress is due to be saved: at most once a second.
pub fn progress_due(last_saved_ms: f64, now_ms: f64) -> bool {
    now_ms - last_saved_ms >= 1_000.0
}

/// The record of `item` for `user`, if any.
pub fn find<'a>(list: &'a [KeptVideo], user: &str, item: &str) -> Option<&'a KeptVideo> {
    list.iter().find(|v| v.user == user && v.item == item)
}

/// Refuses a second download of an item already kept or in progress; a
/// failed one may be started again.
pub fn check_not_kept(list: &[KeptVideo], user: &str, item: &str) -> Result<(), KeepVideoRefusal> {
    match find(list, user, item) {
        Some(v) if !matches!(v.state, KeptState::Failed { .. }) => {
            Err(KeepVideoRefusal::AlreadyKept {
                item: item.to_owned(),
            })
        }
        _ => Ok(()),
    }
}

/// Puts `video` at the front (newest first), replacing the record of the
/// same user and item.
pub fn insert_front(list: &mut Vec<KeptVideo>, video: KeptVideo) {
    list.retain(|v| !(v.user == video.user && v.item == video.item));
    list.insert(0, video);
}

/// Deletes the record of `item` for `user`, returning it.
pub fn take(list: &mut Vec<KeptVideo>, user: &str, item: &str) -> Option<KeptVideo> {
    let at = list.iter().position(|v| v.user == user && v.item == item)?;
    Some(list.remove(at))
}

/// Bytes `user`'s videos hold, leaving out `except` (the item being
/// started, whose old record must not count against itself).
pub fn used_by(list: &[KeptVideo], user: &str, except: Option<&str>) -> u64 {
    list.iter()
        .filter(|v| v.user == user && Some(v.item.as_str()) != except)
        .map(KeptVideo::bytes)
        .sum()
}

/// Bytes `user`'s videos hold or have promised to hold, for admission: a
/// download in progress counts at the larger of what it fetched and what it
/// estimated, so two started together cannot both claim the same room.
/// `except` is left out (the item being started).
pub fn reserved_by(list: &[KeptVideo], user: &str, except: Option<&str>) -> u64 {
    list.iter()
        .filter(|v| v.user == user && Some(v.item.as_str()) != except)
        .map(|v| match v.state {
            KeptState::Downloading { bytes, .. } => bytes.max(v.estimated_bytes),
            _ => v.bytes(),
        })
        .sum()
}

/// Whether a download still has to pass admission: a new one, or a restart
/// of one that closed before its plan was admitted (no estimate recorded).
pub fn needs_admission(resume: bool, recorded_estimate: u64) -> bool {
    !resume || recorded_estimate == 0
}

/// What a download still needs room for: its estimate less what is stored.
pub fn still_needed(estimate: u64, stored: u64) -> u64 {
    estimate.saturating_sub(stored)
}

/// The downloads a page load must restart: the page closed mid-download.
pub fn interrupted(list: &[KeptVideo]) -> Vec<KeptVideo> {
    list.iter()
        .filter(|v| matches!(v.state, KeptState::Downloading { .. }))
        .cloned()
        .collect()
}

/// A new record in the Downloading state, from the request.
pub fn new_record(request: &KeepVideoRequest, saved_at: String) -> KeptVideo {
    KeptVideo {
        item: request.item.clone(),
        version: request.version.clone(),
        user: request.user.clone(),
        title: request.title.clone(),
        kind: request.kind.clone(),
        runtime_ms: request.runtime_ms,
        master_url: request.master_url.clone(),
        audio: request.audio,
        subtitles: request.subtitles.clone(),
        files: Vec::new(),
        quality_bps: request.quality_bps,
        estimated_bytes: 0,
        state: KeptState::Downloading {
            done: 0,
            total: 0,
            bytes: 0,
        },
        position_ms: 0,
        saved_at,
        estimate_from_adoption: false,
        stale_on_disk: 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_download_sends_the_token_only_for_the_user_it_belongs_to() {
        assert_eq!(
            token_for_owner(Some("tok"), Some("u1"), "u1"),
            Ok("tok".to_owned())
        );
        assert_eq!(
            token_for_owner(Some("tok"), Some("u2"), "u1"),
            Err("token_owner_mismatch owner=u1 token_user=u2".to_owned())
        );
        assert_eq!(
            token_for_owner(Some("tok"), None, "u1"),
            Err("token_user_unknown owner=u1".to_owned())
        );
        assert_eq!(
            token_for_owner(None, Some("u1"), "u1"),
            Err("no_token owner=u1".to_owned())
        );
    }

    /// A manifest written before a field existed still parses, and one
    /// written by a newer build (an extra field) does too: a manifest that
    /// does not parse holds its folder and blocks every admission. The
    /// fields that name the item and its files have no default.
    #[test]
    fn an_older_manifest_missing_later_fields_still_parses() {
        let old = r#"{"item":"7","user":"u1","master_url":"http://h/m.m3u8",
            "files":["http://h/m.m3u8"],"state":{"state":"ready"},
            "a_field_from_a_newer_build":1}"#;
        let v: KeptVideo = serde_json::from_str(old).unwrap();
        assert_eq!((v.item.as_str(), v.quality_bps, v.position_ms), ("7", 0, 0));
        assert!(v.subtitles.is_empty() && v.audio.is_none() && v.title.is_empty());
        assert!(v.ready());
        // What names the item or what it stored is not invented.
        for gone in ["item", "user", "master_url", "files", "state"] {
            let mut j: serde_json::Value = serde_json::from_str(old).unwrap();
            j.as_object_mut().unwrap().remove(gone);
            assert!(
                serde_json::from_value::<KeptVideo>(j).is_err(),
                "{gone} defaulted"
            );
        }
    }

    #[test]
    fn a_download_must_fit_under_the_cap_and_above_the_guard() {
        assert_eq!(fits(GIB, 0, 4 * GIB, 10 * GIB), Ok(()));
        assert_eq!(
            fits(2 * GIB, 3 * GIB, 4 * GIB, 10 * GIB),
            Err(KeepVideoRefusal::OverCap {
                needed: 2 * GIB,
                available: GIB
            })
        );
        assert!(matches!(
            fits(GIB, 0, 4 * GIB, 2 * GIB),
            Err(KeepVideoRefusal::LowSpace { .. })
        ));
    }

    #[test]
    fn the_quality_labels_are_distinct() {
        let labels: Vec<_> = QUALITY_CHOICES_BPS
            .iter()
            .map(|b| quality_label(*b))
            .collect();
        assert_eq!(
            labels,
            ["Small (1 Mbps)", "Medium (2 Mbps)", "Large (4 Mbps)"]
        );
    }

    fn video(user: &str, item: &str, state: KeptState) -> KeptVideo {
        new_record(
            &KeepVideoRequest {
                item: item.into(),
                version: String::new(),
                user: user.into(),
                title: "t".into(),
                kind: "movie".into(),
                runtime_ms: 1,
                master_url: "/videos/x/master.m3u8".into(),
                audio: None,
                subtitles: vec![],
                quality_bps: DEFAULT_QUALITY_BPS,
            },
            "2026-01-01T00:00:00Z".into(),
        )
        .with_state(state)
    }

    impl KeptVideo {
        fn with_state(mut self, state: KeptState) -> Self {
            self.state = state;
            self
        }
    }

    #[test]
    fn urls_become_absolute_keys() {
        assert_eq!(
            absolute_url("https://h", "/videos/1/m.m3u8?a=b"),
            "https://h/videos/1/m.m3u8?a=b"
        );
        assert_eq!(absolute_url("https://h/", "/x"), "https://h/x");
        assert_eq!(absolute_url("https://h", "https://o/y"), "https://o/y");
        assert_eq!(
            url_path("https://h/videos/1/0.m4s?PlaySessionId=z"),
            "/videos/1/0.m4s"
        );
        assert_eq!(url_path("/videos/1/0.m4s?a"), "/videos/1/0.m4s");
    }

    #[test]
    fn retries_back_off_then_stop() {
        let delays: Vec<_> = (1..=7).map(retry_delay_secs).collect();
        assert_eq!(
            delays,
            [Some(1), Some(2), Some(4), Some(8), Some(15), Some(30), None]
        );
        assert_eq!(retry_delay_secs(0), None);
        assert!(is_retryable(None) && is_retryable(Some(404)) && is_retryable(Some(500)));
        assert!(!is_retryable(Some(403)) && !is_retryable(Some(401)));
    }

    #[test]
    fn a_restart_skips_what_is_cached() {
        let files: Vec<String> = ["a", "b", "c"].map(String::from).into();
        let cached = ["a", "c"].map(String::from).into();
        assert_eq!(missing_files(&files, &cached), ["b"]);
    }

    #[test]
    fn progress_is_saved_once_a_second() {
        assert!(!progress_due(0.0, 999.0));
        assert!(progress_due(0.0, 1_000.0));
    }

    #[test]
    fn records_are_per_user_and_replace_by_item() {
        let mut list = vec![];
        insert_front(&mut list, video("u1", "a", KeptState::Ready));
        insert_front(&mut list, video("u2", "a", KeptState::Ready));
        insert_front(
            &mut list,
            video("u1", "a", KeptState::Failed { reason: "x".into() }),
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].user, "u1");
        assert!(
            check_not_kept(&list, "u1", "a").is_ok(),
            "failed may restart"
        );
        assert!(matches!(
            check_not_kept(&list, "u2", "a"),
            Err(KeepVideoRefusal::AlreadyKept { .. })
        ));
        assert!(check_not_kept(&list, "u1", "b").is_ok());
        assert!(take(&mut list, "u2", "a").is_some());
        assert!(take(&mut list, "u2", "a").is_none());
    }

    #[test]
    fn used_bytes_leave_out_the_item_being_started() {
        let mut a = video("u", "a", KeptState::Ready);
        a.estimated_bytes = 10;
        let mut b = video(
            "u",
            "b",
            KeptState::Downloading {
                done: 1,
                total: 2,
                bytes: 5,
            },
        );
        b.estimated_bytes = 99;
        let list = vec![a, b];
        assert_eq!(used_by(&list, "u", None), 15);
        assert_eq!(used_by(&list, "u", Some("b")), 10);
        assert_eq!(used_by(&list, "other", None), 0);
        assert_eq!(interrupted(&list).len(), 1);
    }

    #[test]
    fn a_request_key_is_the_path_and_query_whatever_the_spelling() {
        let k = "/videos/1/0.m4s?PlaySessionId=z";
        assert_eq!(request_key(k), k);
        assert_eq!(
            request_key("https://h:8096/videos/1/0.m4s?PlaySessionId=z#f"),
            k
        );
        assert_eq!(request_key("videos/1/0.m4s?PlaySessionId=z"), k);
        assert_eq!(request_key("http://h"), "/");
        // No path: the query stays, under `/`, as the slash spelling has it.
        assert_eq!(
            request_key("https://h?PlaySessionId=z"),
            request_key("/?PlaySessionId=z")
        );
        assert_eq!(
            request_key("https://h:8096?PlaySessionId=z#f"),
            "/?PlaySessionId=z"
        );
        assert_eq!(request_key("https://h#f"), "/");
        // A `/` inside a query with no path before it is not the path.
        assert_eq!(request_key("https://h?next=/a"), "/?next=/a");
        assert_eq!(url_path("https://h?next=/a"), "");
        // A `://` inside the query is not a scheme.
        let redirect = "/videos/1/0.m4s?next=https://x.test/a&PlaySessionId=z";
        assert_eq!(request_key(redirect), redirect);
        assert_eq!(url_path(redirect), "/videos/1/0.m4s");
        assert_eq!(
            request_key("https://h/videos/1/0.m4s?next=https://x.test/a"),
            "/videos/1/0.m4s?next=https://x.test/a"
        );
        assert_eq!(
            absolute_url("https://h", redirect),
            format!("https://h{redirect}")
        );
    }

    #[test]
    fn a_stored_file_is_answered_with_the_type_its_key_names() {
        for (key, want) in [
            ("/videos/1/master.m3u8?a=b", "application/vnd.apple.mpegurl"),
            (
                "/videos/1/h264cmaf/main.m3u8",
                "application/vnd.apple.mpegurl",
            ),
            ("/videos/1/init.mp4?a=b.ts", "video/mp4"),
            ("/videos/1/0.m4s?a=b", "video/mp4"),
            ("/hls1/main/0.ts", "video/mp2t"),
            ("/videos/1/subtitles/3/stream.vtt", "text/vtt"),
            ("/videos/1/stream", "application/octet-stream"),
        ] {
            assert_eq!(content_type_for(key), want, "{key}");
        }
    }

    #[test]
    fn requests_are_classed_and_logged_by_how_often_they_come() {
        let cases = [
            (
                "/videos/1/master.m3u8?p=1",
                RequestClass::Master,
                Some(Loudness::Info),
            ),
            (
                "/videos/1/h264cmaf/main.m3u8?p=1",
                RequestClass::Playlist,
                Some(Loudness::Info),
            ),
            (
                "/videos/1/h264cmaf/init.mp4?p=1",
                RequestClass::Init,
                Some(Loudness::Debug),
            ),
            (
                "/videos/1/h264cmaf/7.m4s?p=1",
                RequestClass::Segment,
                Some(Loudness::Debug),
            ),
            (
                "/hls1/main/7.ts",
                RequestClass::Segment,
                Some(Loudness::Debug),
            ),
            (
                "/videos/1/subtitles/3/stream.vtt",
                RequestClass::Subtitle,
                Some(Loudness::Debug),
            ),
            ("/Items/1/Images/Primary", RequestClass::Other, None),
        ];
        for (key, class, loud) in cases {
            assert_eq!(request_class(key), class, "{key}");
            assert_eq!(loudness(class), loud, "{key}");
        }
    }

    #[test]
    fn the_request_labels_are_distinct_and_the_line_hides_the_query() {
        let mut labels = vec![
            RequestClass::Master.label(),
            RequestClass::Playlist.label(),
            RequestClass::Init.label(),
            RequestClass::Segment.label(),
            RequestClass::Subtitle.label(),
            RequestClass::Other.label(),
            Verdict::ServedOffline.label(),
            Verdict::Network.label(),
        ];
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 8);
        let line = request_line(
            "/videos/1/7.m4s?PlaySessionId=secret",
            Verdict::ServedOffline,
            99,
        );
        assert_eq!(
            line,
            "pharos: video request class=segment verdict=served_offline bytes=99 url_path=/videos/1/7.m4s"
        );
    }

    #[test]
    fn the_cache_is_named_for_the_user() {
        assert_eq!(cache_name("ab"), "pharos-videos-ab");
    }

    /// The worker is JavaScript the page cannot call, so pin the contract
    /// the two share: the cache prefix, the message, the path it serves.
    #[test]
    fn the_service_worker_serves_the_video_caches_by_exact_url() {
        let sw = include_str!("../../assets/sw.js");
        assert!(sw.contains(&format!("const VIDEO_PREFIX = \"{CACHE_PREFIX}\"")));
        assert!(
            sw.contains("\"/ui/assets/vendor/\""),
            "the player script is cached"
        );
        assert!(sw.contains("\"videos-changed\""));
        assert!(sw.contains("cache.match(request.url, { ignoreVary: true })"));
        assert!(
            !sw.contains("ignoreSearch"),
            "the query names the play session"
        );
        assert!(sw.contains("__PHAROS_BUNDLE_HASH__") && sw.contains("__PHAROS_PRECACHE__"));
    }

    #[test]
    fn a_download_in_progress_reserves_its_estimate() {
        let mut a = video(
            "u",
            "a",
            KeptState::Downloading {
                done: 1,
                total: 9,
                bytes: 5,
            },
        );
        a.estimated_bytes = 100;
        let mut b = video(
            "u",
            "b",
            KeptState::Downloading {
                done: 9,
                total: 9,
                bytes: 120,
            },
        );
        b.estimated_bytes = 100;
        let list = vec![a, b];
        assert_eq!(reserved_by(&list, "u", None), 220);
        assert_eq!(reserved_by(&list, "u", Some("a")), 120);
        assert_eq!(
            used_by(&list, "u", None),
            125,
            "display counts fetched bytes"
        );
    }

    #[test]
    fn a_restart_is_admitted_only_when_it_never_was() {
        assert!(needs_admission(false, 500));
        assert!(needs_admission(true, 0));
        assert!(!needs_admission(true, 500));
        assert_eq!(still_needed(100, 30), 70);
        assert_eq!(still_needed(100, 130), 0);
    }

    #[test]
    fn only_a_broken_transfer_is_retried_when_storing() {
        assert!(put_is_retryable("TypeError"));
        assert!(put_is_retryable("NetworkError"));
        assert!(!put_is_retryable("QuotaExceededError"));
        assert!(!put_is_retryable("InvalidStateError"));
    }

    #[test]
    fn a_changed_plan_drops_the_old_files() {
        let old: Vec<String> = ["a", "b", "c"].map(String::from).into();
        let new: Vec<String> = ["b", "d"].map(String::from).into();
        assert_eq!(stale_files(&old, &new), ["a", "c"]);
        assert!(stale_files(&[], &new).is_empty());
    }

    #[test]
    fn the_stored_cap_is_a_choice_or_the_default() {
        assert_eq!(cap_from_stored(None), DEFAULT_CAP_BYTES);
        assert_eq!(cap_from_stored(Some("64")), 64 * GIB);
        assert_eq!(cap_from_stored(Some("3")), DEFAULT_CAP_BYTES);
        assert_eq!(cap_from_stored(Some("lots")), DEFAULT_CAP_BYTES);
    }

    #[test]
    fn cap_options_round_trip_and_press_only_a_choice() {
        for (value, label) in cap_options() {
            let bytes = cap_from_option(&value).unwrap();
            assert_eq!(cap_selected(bytes), value);
            assert_eq!(label, format!("{value} GiB"));
        }
        assert_eq!(cap_selected(DEFAULT_CAP_BYTES), "4");
        assert_eq!(cap_selected(GIB), "");
        assert_eq!(cap_from_option("1"), None);
    }

    #[test]
    fn a_store_without_a_cap_setting_refuses_and_names_the_value() {
        struct Fixed;
        impl VideoStore for Fixed {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                vec![]
            }
            fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                DEFAULT_CAP_BYTES
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
        }
        let err = Fixed.set_cap_bytes(8 * GIB).unwrap_err();
        assert!(err.contains(&(8 * GIB).to_string()), "{err}");
    }
}
