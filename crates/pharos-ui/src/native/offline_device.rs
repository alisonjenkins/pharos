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
pub async fn sync_art(user: &str) -> bool {
    let fence = notes_fence();
    if fence_moved(fence) {
        return true;
    }
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    {
        super::offline_art_web::keep_art(user, &art_of(&load_catalogue(user)), move || {
            !fence_moved(fence)
        })
        .await
    }
    #[cfg(not(all(feature = "web", target_arch = "wasm32")))]
    {
        let _ = user;
        true
    }
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

/// The user whose things are on this device, and the things.
pub fn on_device() -> Option<(String, Vec<OnDevice>)> {
    let mut user = offline_video::last_user();
    let mut kept: Vec<OnDevice> = offline_video::ready_for_last_user()
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
                title: v.title,
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
/// A parent that could not be read leaves `id` marked partial: what was
/// read is kept, and the next pass asks again, walking past the parents
/// already noted until it reaches the one that was missing.
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
    let mut whole = true;
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
            Err(_) => {
                whole = false;
                break;
            }
        }
    }
    trail.reverse();
    catalogue.note(&trail, Some(&detail));
    if whole {
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
}
