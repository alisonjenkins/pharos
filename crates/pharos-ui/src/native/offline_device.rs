//! What this device holds, gathered for the offline library (spec 049,
//! ADR-0149), and the notes about it kept up to date while online.
//!
//! [`device_library`] reads the stores already on the device (downloaded
//! videos, kept books, cached songs) and the catalogue of notes, and builds
//! the [`OfflineLibrary`] the screens read while the server is out of
//! reach. [`fill_notes`] runs once a session is up: it notes each kept item
//! the catalogue lacks (and, once a day, every kept item again), walking up
//! through each parent only until it meets one already noted.
//!
//! Cost model, for TVs and headsets: nothing runs while online but one
//! pass after sign-in, which on a device with nothing kept reads two small
//! stored values and makes no call.

use pharos_client::api::{ItemKind, ItemRef, PharosApi};
use pharos_client::CallInfo;

use super::kept_video::{KeptState, KeptVideo};
use super::offline_library::{Catalogue, OfflineLibrary, OnDevice, STORE_KEY};
use super::offline_video;
use super::platform;

/// Names the browser's poster caches, `pharos-art-<user>` (ADR-0149).
pub const ART_PREFIX: &str = "pharos-art-";

/// The width a poster is stored at; a card at any width is answered with
/// it offline.
pub const ART_WIDTH: u32 = 360;

/// The key a poster is stored under: its URL without the width and format
/// a card asks for, so every card size finds the one stored copy. The
/// service worker makes the same key (`assets/sw.js` `artKey`).
pub fn art_key(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("fillWidth=") && !p.starts_with("format="))
        .collect();
    if kept.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

/// The pictures of every noted item, for the poster store.
pub fn art_of(catalogue: &Catalogue) -> Vec<String> {
    catalogue
        .snaps
        .values()
        .filter(|s| s.kind != "library")
        .filter_map(|s| s.image.clone())
        .collect()
}

/// Brings the browser's posters in line with the notes; nothing elsewhere.
/// Returns whether every poster is stored (always, where none are kept), so
/// a poster that failed is asked again by a later pass.
///
/// Every pass (the one after sign-in or a reconnect, and the periodic one)
/// records its result in one place, [`art_settled`], so a poster that
/// failed in any of them is asked again by the next periodic pass.
pub async fn sync_art(user: &str) -> bool {
    let fence = notes_fence();
    if fence_moved(fence) {
        return true;
    }
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    let settled =
        super::offline_art_web::keep_art(user, &art_of(&load_catalogue(user)), move || {
            !fence_moved(fence)
        })
        .await;
    #[cfg(not(all(feature = "web", target_arch = "wasm32")))]
    let settled = {
        let _ = user;
        true
    };
    ART_SETTLED.store(settled, std::sync::atomic::Ordering::Relaxed);
    settled
}

/// Bumped each time the screens switch library (server or device) or take a
/// new device snapshot, so what was kept for the last one (a grid's pages,
/// a listing's index) is not reused for this one.
static LIBRARY_GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn library_switched() {
    LIBRARY_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn library_generation() -> u32 {
    LIBRARY_GEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the last poster pass stored every poster; false until one has
/// run, so the first periodic pass checks.
static ART_SETTLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn art_settled() -> bool {
    ART_SETTLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The device's library to show in place of `old`, when it changed: the new
/// one, or an empty one once nothing is left on the device (so a removed
/// item does not stay listed). `None` when nothing changed.
pub fn refreshed(old: &OfflineLibrary, now: Option<OfflineLibrary>) -> Option<OfflineLibrary> {
    let now = now.unwrap_or_default();
    (*old != now).then_some(now)
}

/// Bumped as the account signs out, before its notes and posters are
/// forgotten, so a pass still running for it stores nothing after.
static NOTES_FENCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn notes_fence() -> u32 {
    NOTES_FENCE.load(std::sync::atomic::Ordering::Relaxed)
}

fn fence_moved(fence: u32) -> bool {
    notes_fence() != fence
}

/// Forgets the browser's posters, as signing out forgets what is kept.
pub async fn forget_art() {
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    super::offline_art_web::forget_art().await;
}

/// The most parents a note walks up: as `trail::MAX_STEPS`.
const MAX_STEPS: usize = 5;

/// The catalogue of notes for `user`; another user's is never read.
pub fn load_catalogue(user: &str) -> Catalogue {
    match platform::stored_theme(STORE_KEY) {
        Ok(stored) => Catalogue::parse(stored.as_deref()).for_user(user),
        Err(e) => {
            platform::warn(&format!("pharos: offline notes unreadable error={e}"));
            Catalogue::new(user)
        }
    }
}

fn save_catalogue(catalogue: &Catalogue) {
    if let Err(e) = platform::store_theme(STORE_KEY, &catalogue.to_json()) {
        platform::warn(&format!(
            "pharos: offline notes not kept notes={} error={e}",
            catalogue.snaps.len()
        ));
    }
}

/// Forgets every note, as signing out forgets what is kept. A notes or
/// poster pass still in flight then stores nothing.
pub fn forget_catalogue() {
    NOTES_FENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    save_catalogue(&Catalogue::default());
}

/// Whether a downloaded video is listed in the device library, and if not
/// why (V1512). Logged by [`VideoTally::log_line`], so a missing video
/// reads off the log instead of being guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoVerdict {
    /// The signed-in owner's Ready download: listed.
    Listed,
    /// The owner's, but not Ready: it cannot play yet.
    NotReady,
    /// Another account's: never listed.
    OtherOwner,
    /// Nobody confirmed as signed in: nothing is listed.
    IdentityUnconfirmed,
}

impl VideoVerdict {
    pub const ALL: [Self; 4] = [
        Self::Listed,
        Self::NotReady,
        Self::OtherOwner,
        Self::IdentityUnconfirmed,
    ];

    /// The reason string logs and dashboards carry.
    pub fn label(self) -> &'static str {
        match self {
            Self::Listed => "listed",
            Self::NotReady => "not_ready",
            Self::OtherOwner => "other_owner",
            Self::IdentityUnconfirmed => "identity_unconfirmed",
        }
    }
}

/// The verdict for one video, for the confirmed `owner` (`None` until the
/// identity is confirmed).
pub fn judge_video(owner: Option<&str>, video: &KeptVideo) -> VideoVerdict {
    match owner {
        None => VideoVerdict::IdentityUnconfirmed,
        Some(owner) if video.user != owner => VideoVerdict::OtherOwner,
        Some(_) if video.state != KeptState::Ready => VideoVerdict::NotReady,
        Some(_) => VideoVerdict::Listed,
    }
}

/// How many videos got each verdict in one gathering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoTally {
    pub listed: usize,
    pub not_ready: usize,
    pub other_owner: usize,
    pub identity_unconfirmed: usize,
    /// Whether an owner was confirmed at all: with none, there is nothing
    /// to count and the reason is still `identity_unconfirmed`.
    pub owner_known: bool,
}

impl VideoTally {
    /// The first reason that kept something out, else `listed`, else `none`.
    pub fn reason(&self) -> &'static str {
        if !self.owner_known || self.identity_unconfirmed > 0 {
            VideoVerdict::IdentityUnconfirmed.label()
        } else if self.other_owner > 0 {
            VideoVerdict::OtherOwner.label()
        } else if self.not_ready > 0 {
            VideoVerdict::NotReady.label()
        } else if self.listed > 0 {
            VideoVerdict::Listed.label()
        } else {
            "none"
        }
    }

    /// The line the device library logs. Counts only: no account ids, no
    /// titles.
    pub fn log_line(&self) -> String {
        format!(
            "pharos: offline library videos reason={} listed={} not_ready={} other_owner={} identity_unconfirmed={}",
            self.reason(),
            self.listed,
            self.not_ready,
            self.other_owner,
            self.identity_unconfirmed
        )
    }
}

/// The videos the device library lists: the confirmed owner's Ready ones,
/// and the tally of every verdict. A store answers per user, but the owner
/// is checked again here, so a row of another account cannot get in.
pub fn listed_videos<'a>(
    owner: Option<&str>,
    videos: &'a [KeptVideo],
) -> (Vec<&'a KeptVideo>, VideoTally) {
    let mut tally = VideoTally {
        owner_known: owner.is_some(),
        ..VideoTally::default()
    };
    let mut listed = Vec::new();
    for video in videos {
        match judge_video(owner, video) {
            VideoVerdict::Listed => {
                tally.listed += 1;
                listed.push(video);
            }
            VideoVerdict::NotReady => tally.not_ready += 1,
            VideoVerdict::OtherOwner => tally.other_owner += 1,
            VideoVerdict::IdentityUnconfirmed => tally.identity_unconfirmed += 1,
        }
    }
    (listed, tally)
}

thread_local! {
    /// The last videos line logged: gathering runs on every refresh, a line
    /// is for a change.
    static LAST_VIDEOS_LINE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Logs `tally` when it differs from the last one logged; true if it did.
fn log_videos(tally: &VideoTally) -> bool {
    let line = tally.log_line();
    let new = LAST_VIDEOS_LINE.with(|l| {
        let mut l = l.borrow_mut();
        if l.as_deref() == Some(line.as_str()) {
            false
        } else {
            *l = Some(line.clone());
            true
        }
    });
    if new {
        platform::info(&line);
    }
    new
}

/// The user whose things are on this device, and the things.
pub fn on_device() -> Option<(String, Vec<OnDevice>)> {
    let mut user = offline_video::last_user();
    // One read of the owner's Ready downloads, judged against the owner
    // confirmed now.
    let snapshot = offline_video::ready_snapshot();
    let (listed, tally) = listed_videos(user.as_deref(), &snapshot.videos);
    log_videos(&tally);
    let mut kept: Vec<OnDevice> = listed
        .into_iter()
        .filter_map(|v| {
            let id = v.item.parse::<ItemRef>().ok()?;
            Some(OnDevice::Video {
                id,
                kind: if v.kind == "episode" {
                    ItemKind::Episode
                } else {
                    ItemKind::Movie
                },
                title: v.title.clone(),
                runtime_ms: v.runtime_ms,
                position_ms: v.position_ms,
            })
        })
        .collect();
    books_and_songs(&mut user, &mut kept);
    let user = user?;
    Some((user, kept))
}

/// The browser keeps books (spec 036).
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn books_and_songs(user: &mut Option<String>, kept: &mut Vec<OnDevice>) {
    let Some(store) = super::kept_web::read() else {
        return;
    };
    if user.as_deref().is_some_and(|u| u != store.user) {
        return;
    }
    *user = Some(store.user.clone());
    kept.extend(store.books.iter().filter_map(|b| {
        Some(OnDevice::Book {
            id: b.item()?,
            title: b.title.clone(),
            format: match b.format {
                super::kept::KeptFormat::Epub => pharos_client::api::BookFormat::Epub,
                super::kept::KeptFormat::Pdf => pharos_client::api::BookFormat::Pdf,
            },
            fraction: b.fraction,
        })
    }));
}

/// The phone caches songs (spec 044); its store is the signed-in user's.
#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn books_and_songs(_user: &mut Option<String>, kept: &mut Vec<OnDevice>) {
    use super::ports::{ContainerKind, TrackState};
    let Some(store) = super::ports::offline_store() else {
        return;
    };
    for download in store.downloads() {
        let album = match download.container.kind {
            ContainerKind::Album => download.container.id.parse::<ItemRef>().ok(),
            ContainerKind::Mix => None,
        };
        for track in download.tracks {
            if track.state != TrackState::Cached {
                continue;
            }
            if let Ok(id) = track.id.parse::<ItemRef>() {
                kept.push(OnDevice::Song {
                    id,
                    title: track.title,
                    album,
                });
            }
        }
    }
}

/// The library this device holds, or `None` when nothing is on it.
pub fn device_library() -> Option<OfflineLibrary> {
    let (user, kept) = on_device()?;
    if kept.is_empty() {
        return None;
    }
    let catalogue = load_catalogue(&user);
    Some(OfflineLibrary::new(&catalogue, &kept))
}

/// What one [`fill_notes`] pass did, for its log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filled {
    pub kept: usize,
    pub noted: usize,
    pub failed: usize,
    /// Noted, but a parent above could not be read: asked again next pass.
    pub partial: usize,
    pub pruned: usize,
    pub refreshed: bool,
    /// The account signed out during the pass, so nothing was saved.
    pub signed_out: bool,
}

impl Filled {
    pub fn log_line(&self, ms: f64) -> String {
        format!(
            "pharos: offline notes kept={} noted={} failed={} partial={} pruned={} refreshed={} signed_out={} ms={ms:.0}",
            self.kept, self.noted, self.failed, self.partial, self.pruned, self.refreshed, self.signed_out
        )
    }
}

/// Notes `id` and the parents above it the catalogue lacks. Returns false
/// when `id` itself could not be read.
///
/// A trail that does not reach its top (a parent that could not be read,
/// here or when a sibling sharing a parent was noted) leaves `id` marked
/// partial: what was read is kept, and the next pass asks again, walking
/// past the parents already noted until it reaches the one that was
/// missing.
pub async fn note_one<A: PharosApi>(
    api: &A,
    info: &CallInfo,
    catalogue: &mut Catalogue,
    id: ItemRef,
) -> bool {
    let Ok(detail) = api.detail(info, id).await else {
        return false;
    };
    let key = id.to_string();
    let retry = catalogue.partial.contains(&key);
    let mut trail = vec![detail.item.clone()];
    while trail.len() < MAX_STEPS {
        let last = &trail[trail.len() - 1];
        let Some(parent) = last.parent.filter(|_| last.kind != ItemKind::Library) else {
            break;
        };
        if trail.iter().any(|i| i.id == parent) {
            break;
        }
        if !retry && catalogue.snaps.contains_key(&parent.to_string()) {
            break;
        }
        match api.detail(info, parent).await {
            Ok(up) => trail.push(up.item),
            Err(_) => break,
        }
    }
    trail.reverse();
    catalogue.note(&trail, Some(&detail));
    if catalogue.reaches_top(&key) {
        catalogue.partial.remove(&key);
    } else {
        catalogue.partial.insert(key);
    }
    true
}

/// Brings the notes up to date with what is kept: notes what is missing
/// (or everything, once a day), and drops notes nothing kept needs.
pub async fn fill_notes<A: PharosApi>(api: &A, user: &str, now_ms: i64) -> Filled {
    let Some((owner, kept)) = on_device() else {
        return Filled::default();
    };
    if owner != user {
        return Filled::default();
    }
    let fence = notes_fence();
    let ids: Vec<ItemRef> = kept.iter().map(OnDevice::id).collect();
    let mut catalogue = load_catalogue(user);
    let refreshed = !ids.is_empty() && catalogue.due(now_ms);
    let wanted = if refreshed {
        ids.clone()
    } else {
        catalogue.missing(&ids)
    };
    let mut filled = Filled {
        kept: ids.len(),
        refreshed,
        ..Filled::default()
    };
    let info = super::app::call("offline", "notes");
    for id in wanted {
        if note_one(api, &info, &mut catalogue, id).await {
            filled.noted += 1;
        } else {
            filled.failed += 1;
        }
    }
    filled.pruned = catalogue.prune(&ids);
    filled.partial = catalogue.partial.len();
    // A partial trail is asked again on every pass (`Catalogue::missing`),
    // so it need not hold the daily refresh back.
    if refreshed && filled.failed == 0 {
        catalogue.refreshed_ms = now_ms;
    }
    // Signed out while the calls ran: the notes were forgotten, and are not
    // written back.
    if fence_moved(fence) {
        filled.signed_out = true;
        return filled;
    }
    if filled.noted > 0 || filled.pruned > 0 || refreshed {
        save_catalogue(&catalogue);
    }
    filled
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use crate::native::offline_library::Snap;

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut f = std::pin::pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("the fake server never waits"),
        }
    }

    /// A "server" that is itself a library: one show, one season, two
    /// episodes, under a TV library.
    fn server() -> OfflineLibrary {
        let mut c = Catalogue::new("u");
        let snap = |id: &str, kind: &str, name: &str, parent: Option<&str>| Snap {
            id: id.into(),
            kind: kind.into(),
            name: name.into(),
            parent: parent.map(Into::into),
            library_kind: (kind == "library").then(|| "tvshows".to_owned()),
            overview: format!("about {name}"),
            ..Snap::default()
        };
        for s in [
            snap("1", "library", "TV", None),
            snap("s10", "series", "Show", Some("1")),
            snap("s11", "season", "Season 1", Some("s10")),
            snap("101", "episode", "One", Some("s11")),
            snap("102", "episode", "Two", Some("s11")),
        ] {
            c.snaps.insert(s.id.clone(), s);
        }
        let kept = [ItemRef::Real(101), ItemRef::Real(102)].map(|id| OnDevice::Video {
            id,
            title: String::new(),
            kind: ItemKind::Episode,
            runtime_ms: 0,
            position_ms: 0,
        });
        let lib = OfflineLibrary::new(&c, &kept);
        assert_eq!(lib.len(), 5, "the fake holds the whole show");
        lib
    }

    /// Noting an episode walks up to its library; noting the next one stops
    /// at the season already noted, so a second song or episode costs one
    /// call (V1485).
    #[test]
    fn a_note_walks_up_only_to_the_first_parent_already_noted() {
        let server = server();
        let mut c = Catalogue::new("u");
        let info = CallInfo::default();
        assert!(run(note_one(&server, &info, &mut c, ItemRef::Real(101))));
        let keys: Vec<_> = c.snaps.keys().cloned().collect();
        assert_eq!(keys, vec!["1", "101", "s10", "s11"]);
        assert_eq!(c.snaps["101"].overview, "about One");
        // The season is noted, so only the episode is read.
        let before = c.snaps.clone();
        assert!(run(note_one(&server, &info, &mut c, ItemRef::Real(102))));
        assert_eq!(c.snaps.len(), before.len() + 1);
        assert!(!run(note_one(&server, &info, &mut c, ItemRef::Real(999))));
    }

    /// A parent that could not be read leaves the item partial: the next
    /// pass asks for it again and walks past the season already noted to
    /// the missing series; a whole trail is no longer partial.
    #[test]
    fn a_trail_with_a_missing_parent_is_asked_again() {
        // The "server" lost its series: the season's parent cannot be read.
        let mut cut = Catalogue::new("u");
        for (id, kind, name, parent) in [
            ("1", "library", "TV", None),
            ("s11", "season", "Season 1", Some("s10")),
            ("101", "episode", "One", Some("s11")),
        ] {
            cut.snaps.insert(
                id.into(),
                Snap {
                    id: id.into(),
                    kind: kind.into(),
                    name: name.into(),
                    parent: parent.map(Into::into),
                    ..Snap::default()
                },
            );
        }
        let kept = [OnDevice::Video {
            id: ItemRef::Real(101),
            title: String::new(),
            kind: ItemKind::Episode,
            runtime_ms: 0,
            position_ms: 0,
        }];
        let broken = OfflineLibrary::new(&cut, &kept);
        let info = CallInfo::default();
        let mut c = Catalogue::new("u");
        assert!(run(note_one(&broken, &info, &mut c, ItemRef::Real(101))));
        assert!(c.snaps.contains_key("s11"));
        assert!(!c.snaps.contains_key("s10"));
        assert_eq!(c.missing(&[ItemRef::Real(101)]), vec![ItemRef::Real(101)]);
        // The series reads again: the retry walks past the noted season.
        assert!(run(note_one(&server(), &info, &mut c, ItemRef::Real(101))));
        assert!(c.snaps.contains_key("s10"), "{:?}", c.snaps.keys());
        assert!(c.missing(&[ItemRef::Real(101)]).is_empty());
    }

    /// The shown device library follows what is on the device: a change
    /// replaces it, the last item removed empties it, no change keeps it.
    #[test]
    fn the_shown_device_library_follows_the_device() {
        let lib = server();
        assert_eq!(refreshed(&lib, Some(lib.clone())), None);
        let emptied = refreshed(&lib, None).expect("the removed items go");
        assert!(emptied.is_empty());
        assert_eq!(refreshed(&emptied, None), None);
        assert_eq!(refreshed(&emptied, Some(lib.clone())), Some(lib));
    }

    /// Two kept episodes share a season whose series could not be read: both
    /// stay partial, so with the first one gone the next pass still reads
    /// the series for the other.
    #[test]
    fn a_sibling_stopping_at_a_cut_off_parent_is_partial_too() {
        let mut cut = Catalogue::new("u");
        for (id, kind, name, parent) in [
            ("s11", "season", "Season 1", Some("s10")),
            ("101", "episode", "One", Some("s11")),
            ("102", "episode", "Two", Some("s11")),
        ] {
            cut.snaps.insert(
                id.into(),
                Snap {
                    id: id.into(),
                    kind: kind.into(),
                    name: name.into(),
                    parent: parent.map(Into::into),
                    ..Snap::default()
                },
            );
        }
        let kept = [ItemRef::Real(101), ItemRef::Real(102)].map(|id| OnDevice::Video {
            id,
            title: String::new(),
            kind: ItemKind::Episode,
            runtime_ms: 0,
            position_ms: 0,
        });
        let broken = OfflineLibrary::new(&cut, &kept);
        let info = CallInfo::default();
        let mut c = Catalogue::new("u");
        assert!(run(note_one(&broken, &info, &mut c, ItemRef::Real(101))));
        assert!(run(note_one(&broken, &info, &mut c, ItemRef::Real(102))));
        // The first episode is removed; the second still needs the series.
        c.prune(&[ItemRef::Real(102)]);
        assert_eq!(c.missing(&[ItemRef::Real(102)]), vec![ItemRef::Real(102)]);
        assert!(run(note_one(&server(), &info, &mut c, ItemRef::Real(102))));
        assert!(c.snaps.contains_key("s10"), "{:?}", c.snaps.keys());
        assert!(c.missing(&[ItemRef::Real(102)]).is_empty());
    }

    /// Every card width and format finds the one stored poster (V1487).
    #[test]
    fn the_art_key_drops_only_the_width_and_format() {
        let stored = art_key("https://h/Items/7/Images/Primary?tag=ab&fillWidth=360");
        for asked in [
            "https://h/Items/7/Images/Primary?tag=ab&fillWidth=240&format=avif",
            "https://h/Items/7/Images/Primary?fillWidth=480&tag=ab",
            "https://h/Items/7/Images/Primary?tag=ab",
        ] {
            assert_eq!(art_key(asked), stored, "{asked}");
        }
        assert_eq!(stored, "https://h/Items/7/Images/Primary?tag=ab");
        assert_eq!(art_key("https://h/x?fillWidth=1"), "https://h/x");
        assert_ne!(art_key("https://h/Items/7/Images/Primary?tag=cd"), stored);
    }

    #[test]
    fn the_log_line_names_every_count() {
        let filled = Filled {
            kept: 4,
            noted: 2,
            failed: 1,
            partial: 1,
            pruned: 3,
            refreshed: true,
            signed_out: false,
        };
        assert_eq!(
            filled.log_line(12.4),
            "pharos: offline notes kept=4 noted=2 failed=1 partial=1 pruned=3 refreshed=true signed_out=false ms=12"
        );
    }
    fn kept(item: &str, user: &str, state: KeptState) -> KeptVideo {
        KeptVideo {
            item: item.into(),
            version: String::new(),
            user: user.into(),
            title: "Invented Film".into(),
            kind: "movie".into(),
            runtime_ms: 5_400_000,
            master_url: "/m.m3u8".into(),
            audio: None,
            subtitles: vec![],
            files: vec![],
            quality_bps: 2_000_000,
            estimated_bytes: 1_572_864,
            state,
            position_ms: 0,
            saved_at: "2026-11-16T09:00:00Z".into(),
            estimate_from_adoption: false,
            stale_on_disk: 0,
            transferred: None,
            halt: None,
        }
    }

    fn downloading() -> KeptState {
        KeptState::Downloading {
            done: 1,
            total: 4,
            bytes: 10,
        }
    }

    /// V1512: only the confirmed owner's Ready videos are listed; each
    /// other row gets the reason it was kept out.
    #[test]
    fn only_the_owners_ready_videos_are_listed() {
        let rows = [
            kept("7", "A", KeptState::Ready),
            kept("8", "A", downloading()),
            kept("9", "B", KeptState::Ready),
            kept("10", "A", KeptState::Failed { reason: "x".into() }),
        ];
        let (listed, tally) = listed_videos(Some("A"), &rows);
        assert_eq!(
            listed.iter().map(|v| v.item.as_str()).collect::<Vec<_>>(),
            vec!["7"]
        );
        assert_eq!(
            (tally.listed, tally.not_ready, tally.other_owner),
            (1, 2, 1)
        );
        // Nobody confirmed: nothing is listed, whoever owns the rows.
        let (listed, tally) = listed_videos(None, &rows);
        assert!(listed.is_empty());
        assert_eq!(tally.identity_unconfirmed, 4);
        assert_eq!(tally.reason(), "identity_unconfirmed");
        // B is signed in: A's rows are never listed.
        let (listed, _) = listed_videos(Some("B"), &rows);
        assert_eq!(
            listed.iter().map(|v| v.item.as_str()).collect::<Vec<_>>(),
            vec!["9"]
        );
    }

    /// The reason strings are a log contract: four, distinct, stable.
    #[test]
    fn the_video_verdict_labels_are_distinct_and_stable() {
        let labels: Vec<_> = VideoVerdict::ALL.iter().map(|v| v.label()).collect();
        assert_eq!(
            labels,
            ["listed", "not_ready", "other_owner", "identity_unconfirmed"]
        );
        let tally = |rows: &[KeptVideo], owner| listed_videos(owner, rows).1;
        let a = kept("7", "A", KeptState::Ready);
        assert_eq!(
            tally(std::slice::from_ref(&a), Some("A")).reason(),
            "listed"
        );
        assert_eq!(
            tally(std::slice::from_ref(&a), Some("B")).reason(),
            "other_owner"
        );
        assert_eq!(
            tally(&[kept("8", "A", downloading())], Some("A")).reason(),
            "not_ready"
        );
        assert_eq!(tally(&[a], None).reason(), "identity_unconfirmed");
        assert_eq!(tally(&[], Some("A")).reason(), "none");
        let line = tally(&[kept("8", "A", downloading())], Some("A")).log_line();
        assert_eq!(
            line,
            "pharos: offline library videos reason=not_ready listed=0 not_ready=1 other_owner=0 identity_unconfirmed=0"
        );
        assert!(!line.contains("Invented"), "a title reached the log");
    }

    /// One line per change, not one per refresh.
    #[test]
    fn a_repeated_video_verdict_is_logged_once() {
        let rows = [kept("7", "A", KeptState::Ready)];
        let tally = listed_videos(Some("A"), &rows).1;
        assert!(log_videos(&tally));
        assert!(!log_videos(&tally));
        let other = listed_videos(Some("B"), &rows).1;
        assert!(log_videos(&other));
    }

    /// End to end through the gathering the device library is built from:
    /// a store that answers every account's rows still lists only the
    /// signed-in owner's Ready ones, as `OnDevice::Video` items.
    #[test]
    fn the_device_library_lists_the_signed_in_owners_ready_videos() {
        use super::super::kept_video::{KeepVideoRefusal, KeepVideoRequest, VideoStore};
        use super::super::ports::{use_test_video_slot, Slot};
        struct Leaky;
        impl VideoStore for Leaky {
            fn videos(&self, _: &str) -> Vec<KeptVideo> {
                vec![
                    kept("7", "A", KeptState::Ready),
                    kept("8", "A", downloading()),
                    kept("9", "B", KeptState::Ready),
                ]
            }
            fn ready_videos(&self, user: &str) -> Vec<KeptVideo> {
                self.videos(user)
            }
            fn start(&self, _: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
                Ok(())
            }
            fn remove(&self, _: &str, _: &str) {}
            fn save_position(&self, _: &str, _: &str, _: u64) {}
            fn cap_bytes(&self) -> u64 {
                0
            }
            fn used_bytes(&self, _: &str) -> u64 {
                0
            }
            fn free_bytes(&self) -> u64 {
                0
            }
        }
        static SLOT: Slot<dyn VideoStore> = Slot::new();
        SLOT.set(std::sync::Arc::new(Leaky)).ok();
        use_test_video_slot(Some(&SLOT));
        // Nobody is signed in: nothing, so no library.
        offline_video::forget_user();
        assert!(device_library().is_none());
        let _ = platform::take_info_lines();
        offline_video::remember_user("A");
        let (user, kept) = on_device().expect("A's video is on the device");
        // The verdict is logged with its reason and the counts behind it.
        let lines = platform::take_info_lines();
        assert!(
            lines.iter().any(|l| l
                == "pharos: offline library videos reason=other_owner listed=1 not_ready=1 other_owner=1 identity_unconfirmed=0"),
            "{lines:?}"
        );
        assert_eq!(user, "A");
        let ids: Vec<_> = kept
            .iter()
            .filter_map(|k| match k {
                OnDevice::Video { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![ItemRef::Real(7)]);
        assert!(device_library().is_some());
        offline_video::forget_user();
        use_test_video_slot(None);
    }
}
