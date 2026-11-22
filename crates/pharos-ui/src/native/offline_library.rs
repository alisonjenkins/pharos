//! The library as this device holds it (spec 049, ADR-0149): what the
//! screens are given in place of the server's answers while the server
//! cannot be reached.
//!
//! Each kept item (a downloaded video, a kept book, a cached song) is noted
//! in the [`Catalogue`] with its card and page facts and every item above it
//! (season, series, library), read from the server while it was reachable.
//! [`OfflineLibrary`] answers [`PharosApi`]'s reads from that note, holding
//! only what is on the device and what leads to it, so the usual Home,
//! library and title screens draw unchanged and list only what can be
//! played or read now. Anything that needs the server answers
//! [`offline_error`].
//!
//! No I/O here: the platform reads and writes the note
//! ([`super::offline_device::load_catalogue`]), and the app hands in what is
//! on the device.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pharos_client::api::{
    Book, BookFormat, Capabilities, Decision, Detail, Home, HomeRail, Item, ItemKind, ItemRef,
    LibraryKind, ListIndex, Listing, Lyrics, Page, PersonPage, PharosApi, PlaybackGrant,
    ReadPosition, StartTracks, TrackPicks, Unreadable, Watched,
};
use pharos_client::{CallInfo, ClientError};
use serde::{Deserialize, Serialize};

/// Where the catalogue is kept, through `platform::stored_theme`.
pub const STORE_KEY: &str = "pharos.offline.catalogue";

/// The `ClientError::Api` kind every server-only call answers offline.
pub const OFFLINE_KIND: &str = "offline";

/// The deepest a trail is followed up: library, series, season, episode
/// and one spare, as `trail::MAX_STEPS`.
const MAX_DEPTH: usize = 5;

/// How many cards a Home rail holds, as the server's latest rails.
const RAIL_LEN: usize = 24;

/// A video watched this far is finished: it leaves Continue Watching and
/// counts as watched for Next Up.
const FINISHED_PERMILLE: u64 = 950;

/// How often the notes of what is kept are read again while online.
pub const REFRESH_MS: i64 = 24 * 60 * 60 * 1000;

/// How long a dropped connection lasts before the screens switch to the
/// device's library: a blip should not reshuffle the page.
pub const LOST_GRACE_MS: u32 = 5_000;

/// What the app knows about its connection, for [`device_verdict`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Standing {
    /// Connecting or signed in (not at sign-in or a fatal error page).
    pub connecting_or_ready: bool,
    /// Signed in and the connection is up.
    pub connected: bool,
    /// A signed-in session lost its connection and is reconnecting.
    pub reconnecting: bool,
    /// ... for longer than [`LOST_GRACE_MS`].
    pub lost_long: bool,
    /// The first connection is waiting to try again.
    pub waiting: bool,
    /// The first connection has taken a while.
    pub slow_connect: bool,
    /// The browser or OS reports no network.
    pub no_network: bool,
}

/// Whether the screens read the device's library, and why: a stable
/// `reason` label for the log line.
pub fn device_verdict(s: Standing) -> (bool, &'static str) {
    if !s.connecting_or_ready {
        return (false, "not_signed_in");
    }
    if s.connected && !s.reconnecting {
        return (false, "connected");
    }
    if s.no_network {
        return (true, "no_network");
    }
    if s.reconnecting {
        return match s.lost_long {
            true => (true, "connection_lost"),
            false => (false, "reconnecting"),
        };
    }
    if s.waiting {
        return (true, "server_unreachable");
    }
    if s.slow_connect {
        return (true, "slow_connect");
    }
    (false, "connecting")
}

/// The answer to a call that needs the server.
pub fn offline_error(what: &str) -> ClientError {
    ClientError::Api {
        kind: OFFLINE_KIND,
        reason: format!("{what} needs the server, and this device is offline"),
        trace_id: String::new(),
    }
}

fn not_kept(id: ItemRef) -> ClientError {
    ClientError::Api {
        kind: "notFound",
        reason: format!("item {id} is not on this device"),
        trace_id: String::new(),
    }
}

/// An item kind as the catalogue stores it.
pub fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Movie => "movie",
        ItemKind::Episode => "episode",
        ItemKind::Audio => "audio",
        ItemKind::Book => "book",
        ItemKind::Series => "series",
        ItemKind::Season => "season",
        ItemKind::Album => "album",
        ItemKind::Artist => "artist",
        ItemKind::Person => "person",
        ItemKind::Collection => "collection",
        ItemKind::Playlist => "playlist",
        ItemKind::Channel => "channel",
        ItemKind::Library => "library",
        ItemKind::Other => "other",
    }
}

pub fn kind_from_label(label: &str) -> ItemKind {
    match label {
        "movie" => ItemKind::Movie,
        "episode" => ItemKind::Episode,
        "audio" => ItemKind::Audio,
        "book" => ItemKind::Book,
        "series" => ItemKind::Series,
        "season" => ItemKind::Season,
        "album" => ItemKind::Album,
        "artist" => ItemKind::Artist,
        "person" => ItemKind::Person,
        "collection" => ItemKind::Collection,
        "playlist" => ItemKind::Playlist,
        "channel" => ItemKind::Channel,
        "library" => ItemKind::Library,
        _ => ItemKind::Other,
    }
}

fn library_label(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Mixed => "mixed",
        LibraryKind::Movies => "movies",
        LibraryKind::TvShows => "tvshows",
        LibraryKind::Music => "music",
        LibraryKind::Books => "books",
    }
}

fn library_from_label(label: &str) -> LibraryKind {
    match label {
        "movies" => LibraryKind::Movies,
        "tvshows" => LibraryKind::TvShows,
        "music" => LibraryKind::Music,
        "books" => LibraryKind::Books,
        _ => LibraryKind::Mixed,
    }
}

fn format_label(format: BookFormat) -> &'static str {
    match format {
        BookFormat::Epub => "epub",
        BookFormat::Pdf => "pdf",
        BookFormat::Comic => "comic",
        BookFormat::Unreadable => "unreadable",
    }
}

fn format_from_label(label: &str) -> BookFormat {
    match label {
        "epub" => BookFormat::Epub,
        "pdf" => BookFormat::Pdf,
        "comic" => BookFormat::Comic,
        _ => BookFormat::Unreadable,
    }
}

/// One item as noted for offline: its card, and its page's text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snap {
    pub id: String,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default)]
    pub runtime_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default)]
    pub played: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default)]
    pub audiobook: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub overview: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// A book's format; `None` for anything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_format: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
}

impl Snap {
    /// The note for `item`, with its page's text when `detail` is given.
    pub fn of(item: &Item, detail: Option<&Detail>) -> Self {
        Self {
            id: item.id.to_string(),
            kind: kind_label(item.kind).to_owned(),
            name: item.name.clone(),
            year: item.year,
            runtime_ms: item.runtime_ms,
            image: item.image.clone(),
            played: item.played,
            series_name: item.series_name.clone(),
            season: item.season,
            episode: item.episode,
            parent: item.parent.map(|p| p.to_string()),
            library_kind: item.library_kind.map(|k| library_label(k).to_owned()),
            artist: item.artist.clone(),
            audiobook: item.audiobook,
            overview: detail.map(|d| d.overview.clone()).unwrap_or_default(),
            genres: detail.map(|d| d.genres.clone()).unwrap_or_default(),
            book_format: detail
                .and_then(|d| d.book.as_ref())
                .map(|b| format_label(b.format).to_owned()),
            author: detail
                .and_then(|d| d.book.as_ref())
                .map(|b| b.author.clone())
                .unwrap_or_default(),
        }
    }

    pub fn item_ref(&self) -> Option<ItemRef> {
        self.id.parse().ok()
    }

    fn parent_ref(&self) -> Option<ItemRef> {
        self.parent.as_deref().and_then(|p| p.parse().ok())
    }
}

/// Every noted item, for one user.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalogue {
    pub user: String,
    #[serde(default)]
    pub snaps: BTreeMap<String, Snap>,
    /// When every kept item's note was last read again from the server,
    /// unix ms on this device's clock; 0 when never.
    #[serde(default)]
    pub refreshed_ms: i64,
    /// Kept items whose trail stopped at a parent that could not be read;
    /// each is asked again on the next pass.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub partial: BTreeSet<String>,
}

impl Catalogue {
    pub fn new(user: &str) -> Self {
        Self {
            user: user.to_owned(),
            snaps: BTreeMap::new(),
            refreshed_ms: 0,
            partial: BTreeSet::new(),
        }
    }

    /// The record as stored; a missing or unreadable one is empty.
    pub fn parse(stored: Option<&str>) -> Self {
        stored
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// The catalogue `user` may read: another user's is never handed out.
    pub fn for_user(self, user: &str) -> Self {
        if self.user == user {
            self
        } else {
            Self::new(user)
        }
    }

    /// Notes a trail (outermost first, as `trail::load` returns it) with
    /// the page text of its last item. Returns whether anything changed.
    pub fn note(&mut self, trail: &[Item], detail: Option<&Detail>) -> bool {
        let mut changed = false;
        let last = trail.len().saturating_sub(1);
        for (i, item) in trail.iter().enumerate() {
            let snap = Snap::of(item, if i == last { detail } else { None });
            let key = snap.id.clone();
            // An ancestor noted with its own page text keeps it.
            let snap = match self.snaps.get(&key) {
                Some(old) if i != last && !old.overview.is_empty() => Snap {
                    overview: old.overview.clone(),
                    genres: old.genres.clone(),
                    ..snap
                },
                _ => snap,
            };
            if self.snaps.get(&key) != Some(&snap) {
                self.snaps.insert(key, snap);
                changed = true;
            }
        }
        changed
    }

    /// Notes items listed under one parent (an album's songs). Returns
    /// whether anything changed.
    pub fn note_children(&mut self, items: &[Item]) -> bool {
        let mut changed = false;
        for item in items {
            let snap = Snap::of(item, None);
            let keep_text = self
                .snaps
                .get(&snap.id)
                .map(|old| (old.overview.clone(), old.genres.clone()));
            let snap = match keep_text {
                Some((overview, genres)) => Snap {
                    overview,
                    genres,
                    ..snap
                },
                None => snap,
            };
            if self.snaps.get(&snap.id) != Some(&snap) {
                self.snaps.insert(snap.id.clone(), snap);
                changed = true;
            }
        }
        changed
    }

    /// Of `kept`, the ids with no note yet, or whose parents were only
    /// partly read: what a reconnect fills in.
    pub fn missing(&self, kept: &[ItemRef]) -> Vec<ItemRef> {
        kept.iter()
            .filter(|id| {
                let key = id.to_string();
                !self.snaps.contains_key(&key) || self.partial.contains(&key)
            })
            .copied()
            .collect()
    }

    /// Whether every kept item's note is due to be read again: once a day,
    /// so a mark made elsewhere reaches Next Up here.
    pub fn due(&self, now_ms: i64) -> bool {
        self.refreshed_ms == 0 || now_ms.saturating_sub(self.refreshed_ms) >= REFRESH_MS
    }

    /// Drops every note that is neither `kept` nor above a kept item.
    /// Returns how many were dropped.
    pub fn prune(&mut self, kept: &[ItemRef]) -> usize {
        let mut wanted = BTreeSet::new();
        for id in kept {
            let mut at = Some(id.to_string());
            let mut depth = 0;
            while let Some(key) = at.take() {
                if depth > MAX_DEPTH || !wanted.insert(key.clone()) {
                    break;
                }
                depth += 1;
                at = self.snaps.get(&key).and_then(|s| s.parent.clone());
            }
        }
        let before = self.snaps.len();
        self.snaps.retain(|key, _| wanted.contains(key));
        let kept: BTreeSet<String> = kept.iter().map(ToString::to_string).collect();
        self.partial.retain(|key| kept.contains(key));
        before - self.snaps.len()
    }
}

/// One thing on this device that plays or opens with no server.
#[derive(Debug, Clone, PartialEq)]
pub enum OnDevice {
    /// A downloaded film or episode, and where it was left on the device.
    Video {
        id: ItemRef,
        title: String,
        kind: ItemKind,
        runtime_ms: u64,
        position_ms: u64,
    },
    /// A kept book, its format, and how far through it was read on the
    /// device.
    Book {
        id: ItemRef,
        title: String,
        format: BookFormat,
        fraction: f32,
    },
    /// A cached song, and the album it was kept with.
    Song {
        id: ItemRef,
        title: String,
        album: Option<ItemRef>,
    },
}

impl OnDevice {
    /// The kind of library it belongs in when no note says.
    fn library_kind(&self) -> LibraryKind {
        match self {
            Self::Video {
                kind: ItemKind::Episode,
                ..
            } => LibraryKind::TvShows,
            Self::Video { .. } => LibraryKind::Movies,
            Self::Book { .. } => LibraryKind::Books,
            Self::Song { .. } => LibraryKind::Music,
        }
    }

    pub fn id(&self) -> ItemRef {
        match self {
            Self::Video { id, .. } | Self::Book { id, .. } | Self::Song { id, .. } => *id,
        }
    }
}

fn blank_item(id: ItemRef, kind: ItemKind, name: &str) -> Item {
    Item {
        id,
        kind,
        name: name.to_owned(),
        year: None,
        runtime_ms: 0,
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
        audiobook: false,
    }
}

fn snap_item(snap: &Snap, id: ItemRef) -> Item {
    Item {
        year: snap.year,
        runtime_ms: snap.runtime_ms,
        image: snap.image.clone(),
        played: snap.played,
        series_name: snap.series_name.clone(),
        season: snap.season,
        episode: snap.episode,
        parent: snap.parent_ref(),
        library_kind: snap.library_kind.as_deref().map(library_from_label),
        artist: snap.artist.clone(),
        audiobook: snap.audiobook,
        ..blank_item(id, kind_from_label(&snap.kind), &snap.name)
    }
}

fn finished(position_ms: u64, runtime_ms: u64) -> bool {
    runtime_ms > 0
        && u128::from(position_ms) * 1000 >= u128::from(runtime_ms) * u128::from(FINISHED_PERMILLE)
}

/// The reads the screens make, answered from the device.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OfflineLibrary {
    /// Every item shown: what is on the device and what leads to it.
    items: HashMap<ItemRef, Item>,
    /// Page text by id.
    text: HashMap<ItemRef, Snap>,
    /// Children by parent, in insertion order.
    children: HashMap<ItemRef, Vec<ItemRef>>,
    /// Libraries, in Home's order.
    libraries: Vec<ItemRef>,
    /// Films and episodes part watched on the device, newest kept first.
    resume: Vec<ItemRef>,
    /// Books part read on the device.
    reading: Vec<ItemRef>,
    /// Where each book was left on the device.
    places: HashMap<ItemRef, f32>,
    /// Each kept book's format, for a book with no note.
    formats: HashMap<ItemRef, BookFormat>,
}

/// The name of a library made up for kept items with no note.
fn library_name(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::TvShows => "TV Shows",
        LibraryKind::Movies => "Movies",
        LibraryKind::Music => "Music",
        LibraryKind::Books => "Books",
        LibraryKind::Mixed => "Media",
    }
}

/// Home's library order: TV Shows, Movies, Music, Books, then the rest.
fn library_rank(kind: Option<LibraryKind>) -> u8 {
    match kind {
        Some(LibraryKind::TvShows) => 0,
        Some(LibraryKind::Movies) => 1,
        Some(LibraryKind::Music) => 2,
        Some(LibraryKind::Books) => 3,
        Some(LibraryKind::Mixed) | None => 4,
    }
}

impl OfflineLibrary {
    /// The library holding `on_device`, described by `catalogue`. An item
    /// the catalogue has no note of shows with its kept title only (it was
    /// kept by a build before notes, or never reached the server since).
    pub fn new(catalogue: &Catalogue, on_device: &[OnDevice]) -> Self {
        let mut lib = Self::default();
        let mut order: Vec<ItemRef> = Vec::new();
        for kept in on_device {
            let id = kept.id();
            let mut item = match catalogue.snaps.get(&id.to_string()) {
                Some(snap) => {
                    lib.text.insert(id, snap.clone());
                    snap_item(snap, id)
                }
                None => match kept {
                    OnDevice::Video {
                        title,
                        kind,
                        runtime_ms,
                        ..
                    } => Item {
                        runtime_ms: *runtime_ms,
                        ..blank_item(id, *kind, title)
                    },
                    OnDevice::Book { title, .. } => blank_item(id, ItemKind::Book, title),
                    OnDevice::Song { title, album, .. } => Item {
                        parent: *album,
                        ..blank_item(id, ItemKind::Audio, title)
                    },
                },
            };
            match kept {
                OnDevice::Video {
                    position_ms,
                    runtime_ms,
                    ..
                } => {
                    let runtime = if item.runtime_ms > 0 {
                        item.runtime_ms
                    } else {
                        *runtime_ms
                    };
                    if finished(*position_ms, runtime) {
                        item.played = true;
                        item.position_ms = 0;
                    } else {
                        item.position_ms = *position_ms;
                        if *position_ms > 0 {
                            lib.resume.push(id);
                        }
                    }
                }
                OnDevice::Book {
                    fraction, format, ..
                } => {
                    lib.formats.insert(id, *format);
                    lib.places.insert(id, *fraction);
                    if *fraction > 0.0 && *fraction < 1.0 {
                        lib.reading.push(id);
                    }
                }
                OnDevice::Song { .. } => {}
            }
            if lib.items.insert(id, item).is_none() {
                order.push(id);
            }
        }
        // Walk each kept item up to its library.
        for id in order.clone() {
            let mut child = id;
            for _ in 0..MAX_DEPTH {
                let Some(parent) = lib.items.get(&child).and_then(|i| i.parent) else {
                    break;
                };
                if !lib.items.contains_key(&parent) {
                    let Some(snap) = catalogue.snaps.get(&parent.to_string()) else {
                        break;
                    };
                    lib.text.insert(parent, snap.clone());
                    lib.items.insert(parent, snap_item(snap, parent));
                    order.push(parent);
                }
                if lib
                    .items
                    .get(&child)
                    .is_some_and(|i| i.kind == ItemKind::Library)
                {
                    break;
                }
                child = parent;
            }
        }
        for id in &order {
            let Some(item) = lib.items.get(id) else {
                continue;
            };
            if item.kind == ItemKind::Library {
                lib.libraries.push(*id);
                continue;
            }
            if let Some(parent) = item.parent.filter(|p| lib.items.contains_key(p)) {
                let list = lib.children.entry(parent).or_default();
                if !list.contains(id) {
                    list.push(*id);
                }
            }
        }
        // A kept item with no note up to a library (kept since the last
        // notes pass, or never noted) still shows: its top joins a library
        // of its kind, made up when the device has none noted.
        for kept in on_device {
            let mut top = kept.id();
            for _ in 0..MAX_DEPTH {
                match lib.items.get(&top).and_then(|i| i.parent) {
                    Some(parent) if lib.items.contains_key(&parent) => top = parent,
                    _ => break,
                }
            }
            if lib
                .items
                .get(&top)
                .is_some_and(|i| i.kind == ItemKind::Library)
            {
                continue;
            }
            let kind = kept.library_kind();
            let library = match lib
                .libraries
                .iter()
                .find(|l| lib.items.get(l).and_then(|i| i.library_kind) == Some(kind))
            {
                Some(found) => *found,
                None => {
                    let id = ItemRef::Synth(u64::MAX - u64::from(library_rank(Some(kind))));
                    lib.items.insert(
                        id,
                        Item {
                            library_kind: Some(kind),
                            ..blank_item(id, ItemKind::Library, library_name(kind))
                        },
                    );
                    lib.libraries.push(id);
                    id
                }
            };
            let list = lib.children.entry(library).or_default();
            if !list.contains(&top) {
                list.push(top);
            }
        }
        let items = &lib.items;
        lib.libraries
            .sort_by_key(|id| library_rank(items.get(id).and_then(|i| i.library_kind)));
        lib
    }

    /// Whether nothing is on the device.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many items it shows, kept ones and what leads to them.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    fn item(&self, id: ItemRef) -> Option<Item> {
        self.items.get(&id).cloned()
    }

    /// Everything under `id`, depth first, `id` itself left out.
    fn descendants(&self, id: ItemRef) -> Vec<ItemRef> {
        let mut out = Vec::new();
        let mut stack = vec![(id, 0usize)];
        while let Some((at, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                continue;
            }
            for child in self.children.get(&at).into_iter().flatten().rev() {
                out.push(*child);
                stack.push((*child, depth + 1));
            }
        }
        out
    }

    /// The episodes of a series in season and episode order.
    fn episodes(&self, series: ItemRef) -> Vec<Item> {
        let mut eps: Vec<Item> = self
            .descendants(series)
            .into_iter()
            .filter_map(|id| self.item(id))
            .filter(|i| i.kind == ItemKind::Episode)
            .collect();
        eps.sort_by_key(|e| (e.season.unwrap_or(0), e.episode.unwrap_or(0)));
        eps
    }

    /// For each series with a watched or started episode on the device, the
    /// first episode after the last one watched that is on the device and
    /// not watched.
    fn next_up(&self) -> Vec<Item> {
        let mut out = Vec::new();
        for id in self.items.keys() {
            if self.items.get(id).map(|i| i.kind) != Some(ItemKind::Series) {
                continue;
            }
            let eps = self.episodes(*id);
            let Some(last) = eps.iter().rposition(|e| e.played) else {
                continue;
            };
            if let Some(next) = eps
                .iter()
                .skip(last + 1)
                .find(|e| !e.played && e.position_ms == 0)
            {
                out.push(next.clone());
            }
        }
        out.sort_by(|a, b| a.series_name.cmp(&b.series_name));
        out
    }

    /// The cards a library's Home rail shows: its own children, in name
    /// order.
    fn rail(&self, library: ItemRef) -> Vec<Item> {
        let mut items: Vec<Item> = self
            .children
            .get(&library)
            .into_iter()
            .flatten()
            .filter_map(|id| self.item(*id))
            .collect();
        sort_items(&mut items, "SortName", false);
        items.truncate(RAIL_LEN);
        items
    }

    fn listing(&self, listing: &Listing) -> Vec<Item> {
        let ids = match listing.group {
            // A grouped listing (a library's series or albums) lists every
            // item of that kind under the parent, however deep.
            // A kept item with no note sits straight under its library
            // (a bare episode or song); it is listed too, unless something
            // of the grouped kind under it is listed already.
            Some(group) => {
                let of_group = |id: &ItemRef| self.items.get(id).is_some_and(|i| i.kind == group);
                let mut ids: Vec<ItemRef> = self
                    .descendants(listing.parent)
                    .into_iter()
                    .filter(of_group)
                    .collect();
                for child in self.children.get(&listing.parent).into_iter().flatten() {
                    if !of_group(child) && !self.descendants(*child).iter().any(of_group) {
                        ids.push(*child);
                    }
                }
                ids
            }
            None => self
                .children
                .get(&listing.parent)
                .cloned()
                .unwrap_or_default(),
        };
        let mut items: Vec<Item> = ids
            .into_iter()
            .filter_map(|id| self.item(id))
            .filter(|i| match listing.watched {
                None => true,
                Some(Watched::Watched) => i.played,
                Some(Watched::Unwatched) => !i.played,
                Some(Watched::InProgress) => i.position_ms > 0,
            })
            .filter(|i| match &listing.genre {
                None => true,
                Some(genre) => self
                    .text
                    .get(&i.id)
                    .is_some_and(|s| s.genres.contains(genre)),
            })
            .filter(|i| {
                let unreadable = self.text.get(&i.id).and_then(|s| s.book_format.as_deref())
                    == Some("unreadable");
                match listing.unreadable {
                    Unreadable::Show => true,
                    Unreadable::Hide => !unreadable,
                    Unreadable::Only => unreadable,
                }
            })
            .collect();
        sort_items(&mut items, &listing.sort_by, listing.descending);
        items
    }

    fn detail_of(&self, id: ItemRef) -> Option<Detail> {
        let item = self.item(id)?;
        let snap = self.text.get(&id);
        let book = snap
            .and_then(|s| {
                s.book_format.as_deref().map(|f| Book {
                    format: format_from_label(f),
                    author: s.author.clone(),
                    page_count: 0,
                })
            })
            .or_else(|| {
                self.formats.get(&id).map(|format| Book {
                    format: *format,
                    author: String::new(),
                    page_count: 0,
                })
            });
        let read_position = self
            .places
            .get(&id)
            .filter(|f| **f > 0.0)
            .map(|f| ReadPosition {
                locator: String::new(),
                fraction: *f,
                read_at_ms: 0,
            });
        Some(Detail {
            overview: snap.map(|s| s.overview.clone()).unwrap_or_default(),
            genres: snap.map(|s| s.genres.clone()).unwrap_or_default(),
            chapters: Vec::new(),
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            trickplay: Vec::new(),
            segments: Vec::new(),
            fonts: Vec::new(),
            artists: item.artist.iter().cloned().collect(),
            album: None,
            cast: Vec::new(),
            versions: Vec::new(),
            damage: None,
            book,
            read_position,
            item,
        })
    }
}

fn sort_key_name(item: &Item) -> String {
    let name = item.name.to_lowercase();
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = name.strip_prefix(article) {
            return rest.to_owned();
        }
    }
    name
}

fn sort_items(items: &mut [Item], sort_by: &str, descending: bool) {
    if sort_by.contains("IndexNumber") {
        items.sort_by_key(|i| {
            (
                i.season.unwrap_or(0),
                i.episode.unwrap_or(0),
                sort_key_name(i),
            )
        });
    } else if sort_by.contains("ProductionYear") || sort_by.contains("PremiereDate") {
        items.sort_by_key(|i| (i.year.unwrap_or(0), sort_key_name(i)));
    } else {
        items.sort_by_key(sort_key_name);
    }
    if descending {
        items.reverse();
    }
}

fn page(items: Vec<Item>, start: u32, limit: u32) -> Page {
    let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
    let take = if limit == 0 {
        usize::MAX
    } else {
        limit as usize
    };
    Page {
        items: items.into_iter().skip(start as usize).take(take).collect(),
        total,
        start,
    }
}

fn first_letter(item: &Item) -> String {
    match sort_key_name(item).chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase().to_string(),
        _ => "#".to_owned(),
    }
}

/// Never made: nothing plays through the server offline.
pub struct NoGrant;

impl PlaybackGrant for NoGrant {
    async fn report(
        &self,
        _: &CallInfo,
        _: u64,
        _: bool,
        _: TrackPicks,
    ) -> Result<(), ClientError> {
        Err(offline_error("progress"))
    }
    async fn stop(&self, _: &CallInfo, _: u64) -> Result<(), ClientError> {
        Err(offline_error("progress"))
    }
}

impl PharosApi for OfflineLibrary {
    type Playing = NoGrant;

    async fn home(&self, _: &CallInfo) -> Result<Home, ClientError> {
        let rails = self
            .libraries
            .iter()
            .filter_map(|id| {
                let latest = self.rail(*id);
                let library = self.item(*id)?;
                (!latest.is_empty()).then_some(HomeRail { library, latest })
            })
            .collect();
        Ok(Home {
            rails,
            resume: self
                .resume
                .iter()
                .rev()
                .filter_map(|id| self.item(*id))
                .collect(),
            listening: Vec::new(),
            reading: self
                .reading
                .iter()
                .filter_map(|id| self.item(*id))
                .collect(),
            next_up: self.next_up(),
        })
    }

    async fn views(&self, _: &CallInfo) -> Result<Page, ClientError> {
        let items: Vec<Item> = self
            .libraries
            .iter()
            .filter(|id| self.children.get(id).is_some_and(|c| !c.is_empty()))
            .filter_map(|id| self.item(*id))
            .collect();
        Ok(page(items, 0, 0))
    }

    async fn query(
        &self,
        _: &CallInfo,
        listing: &Listing,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError> {
        Ok(page(self.listing(listing), start, limit))
    }

    async fn index(&self, _: &CallInfo, listing: &Listing) -> Result<ListIndex, ClientError> {
        // The letters jump into the listing as shown (genre applied); the
        // genre picker offers every genre of the unfiltered listing, as the
        // server does.
        let items = self.listing(listing);
        let all = self.listing(&Listing {
            genre: None,
            ..listing.clone()
        });
        let mut letters: Vec<(String, u32)> = Vec::new();
        for (n, item) in items.iter().enumerate() {
            let letter = first_letter(item);
            if letters.last().map(|(l, _)| l) != Some(&letter) {
                letters.push((letter, u32::try_from(n).unwrap_or(u32::MAX)));
            }
        }
        let genres: BTreeSet<String> = all
            .iter()
            .filter_map(|i| self.text.get(&i.id))
            .flat_map(|s| s.genres.iter().cloned())
            .collect();
        Ok(ListIndex {
            letters,
            genres: genres.into_iter().collect(),
        })
    }

    async fn search(&self, _: &CallInfo, text: &str, limit: u32) -> Result<Page, ClientError> {
        let needle = text.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Page::default());
        }
        let mut hits: Vec<Item> = self
            .items
            .values()
            .filter(|i| i.kind != ItemKind::Library)
            .filter(|i| {
                i.name.to_lowercase().contains(&needle)
                    || i.series_name
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&needle))
                    || i.artist
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&needle))
            })
            .cloned()
            .collect();
        // Names that start with the text first, then by name.
        hits.sort_by_key(|i| {
            (
                !i.name.to_lowercase().starts_with(&needle),
                sort_key_name(i),
            )
        });
        Ok(page(hits, 0, limit))
    }

    async fn detail(&self, _: &CallInfo, id: ItemRef) -> Result<Detail, ClientError> {
        self.detail_of(id).ok_or_else(|| not_kept(id))
    }

    async fn person(&self, _: &CallInfo, _: ItemRef) -> Result<PersonPage, ClientError> {
        Err(offline_error("a person's page"))
    }

    async fn lyrics(&self, _: &CallInfo, _: ItemRef, _: bool) -> Result<Lyrics, ClientError> {
        Err(offline_error("lyrics"))
    }

    async fn mix(
        &self,
        _: &CallInfo,
        _: &pharos_client::mix::MixSpec,
        _: &[ItemRef],
        _: u32,
    ) -> Result<pharos_client::mix::MixBatch, ClientError> {
        Err(offline_error("a mix"))
    }

    async fn play(
        &self,
        _: &CallInfo,
        _: ItemRef,
        _: &str,
        _: &Capabilities,
        _: TrackPicks,
    ) -> Result<(Decision, NoGrant), ClientError> {
        Err(offline_error("streaming"))
    }

    async fn start_tracks(
        &self,
        _: &CallInfo,
        _: ItemRef,
        _: &Capabilities,
    ) -> Result<StartTracks, ClientError> {
        Err(offline_error("streaming"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut f = std::pin::pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("the offline library never waits"),
        }
    }

    const TV: ItemRef = ItemRef::Real(1);
    const FILMS: ItemRef = ItemRef::Real(2);
    const BOOKS: ItemRef = ItemRef::Real(3);
    const SHOW: ItemRef = ItemRef::Synth(10);
    const S1: ItemRef = ItemRef::Synth(11);
    const E1: ItemRef = ItemRef::Real(101);
    const E2: ItemRef = ItemRef::Real(102);
    const E3: ItemRef = ItemRef::Real(103);
    const FILM: ItemRef = ItemRef::Real(201);
    const OTHER_FILM: ItemRef = ItemRef::Real(202);
    const BOOK: ItemRef = ItemRef::Real(301);

    fn library(id: ItemRef, kind: LibraryKind, name: &str) -> Item {
        Item {
            library_kind: Some(kind),
            ..blank_item(id, ItemKind::Library, name)
        }
    }

    fn under(id: ItemRef, kind: ItemKind, name: &str, parent: ItemRef) -> Item {
        Item {
            parent: Some(parent),
            ..blank_item(id, kind, name)
        }
    }

    fn episode(id: ItemRef, n: u32) -> Item {
        Item {
            season: Some(1),
            episode: Some(n),
            series_name: Some("Show".into()),
            runtime_ms: 1_000_000,
            ..under(id, ItemKind::Episode, &format!("Episode {n}"), S1)
        }
    }

    /// A page for `item` with `overview`, made the way the library makes one.
    fn detail_with(item: &Item, overview: &str) -> Detail {
        let mut c = Catalogue::new("u");
        c.note(std::slice::from_ref(item), None);
        let kept = [OnDevice::Book {
            id: item.id,
            title: item.name.clone(),
            format: BookFormat::Epub,
            fraction: 0.0,
        }];
        let mut d = OfflineLibrary::new(&c, &kept).detail_of(item.id).unwrap();
        d.overview = overview.into();
        d
    }

    fn catalogue() -> Catalogue {
        let mut c = Catalogue::new("u");
        let tv = library(TV, LibraryKind::TvShows, "TV Shows");
        let show = under(SHOW, ItemKind::Series, "Show", TV);
        let s1 = under(S1, ItemKind::Season, "Season 1", SHOW);
        for n in 1..=3 {
            let e = episode(ItemRef::Real(100 + u64::from(n)), n);
            c.note(&[tv.clone(), show.clone(), s1.clone(), e], None);
        }
        let films = library(FILMS, LibraryKind::Movies, "Movies");
        let film = Item {
            year: Some(1994),
            runtime_ms: 6_000_000,
            ..under(FILM, ItemKind::Movie, "The Film", FILMS)
        };
        c.note(
            &[films.clone(), film.clone()],
            Some(&detail_with(&film, "A film.")),
        );
        let other = under(OTHER_FILM, ItemKind::Movie, "Another", FILMS);
        c.note(&[films, other], None);
        let books = library(BOOKS, LibraryKind::Books, "Books");
        c.note(&[books, under(BOOK, ItemKind::Book, "A Book", BOOKS)], None);
        c
    }

    fn video(id: ItemRef, kind: ItemKind, position_ms: u64) -> OnDevice {
        OnDevice::Video {
            id,
            title: format!("kept {id}"),
            kind,
            runtime_ms: 1_000_000,
            position_ms,
        }
    }

    fn device() -> Vec<OnDevice> {
        vec![
            video(E1, ItemKind::Episode, 999_000),
            video(E2, ItemKind::Episode, 0),
            video(E3, ItemKind::Episode, 0),
            video(FILM, ItemKind::Movie, 1_200_000),
            OnDevice::Book {
                id: BOOK,
                title: "A Book".into(),
                format: BookFormat::Epub,
                fraction: 0.3,
            },
        ]
    }

    fn info() -> CallInfo {
        CallInfo::default()
    }

    #[test]
    fn the_device_stands_in_only_once_the_server_is_out_of_reach() {
        let base = Standing {
            connecting_or_ready: true,
            ..Standing::default()
        };
        let cases = [
            (
                Standing {
                    connecting_or_ready: false,
                    no_network: true,
                    ..base
                },
                (false, "not_signed_in"),
            ),
            (
                Standing {
                    connected: true,
                    ..base
                },
                (false, "connected"),
            ),
            (
                Standing {
                    connected: true,
                    no_network: true,
                    ..base
                },
                (false, "connected"),
            ),
            (
                Standing {
                    no_network: true,
                    ..base
                },
                (true, "no_network"),
            ),
            (
                Standing {
                    reconnecting: true,
                    ..base
                },
                (false, "reconnecting"),
            ),
            (
                Standing {
                    reconnecting: true,
                    lost_long: true,
                    ..base
                },
                (true, "connection_lost"),
            ),
            (
                Standing {
                    waiting: true,
                    ..base
                },
                (true, "server_unreachable"),
            ),
            (
                Standing {
                    slow_connect: true,
                    ..base
                },
                (true, "slow_connect"),
            ),
            (base, (false, "connecting")),
        ];
        for (standing, want) in cases {
            assert_eq!(device_verdict(standing), want, "{standing:?}");
        }
    }

    #[test]
    fn notes_are_due_again_after_a_day() {
        let mut c = Catalogue::new("u");
        assert!(c.due(1));
        c.refreshed_ms = 1_000;
        assert!(!c.due(1_000 + REFRESH_MS - 1));
        assert!(c.due(1_000 + REFRESH_MS));
    }

    #[test]
    fn home_rails_hold_only_what_is_on_the_device_in_home_order() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let home = run(lib.home(&info())).unwrap();
        let rails: Vec<_> = home
            .rails
            .iter()
            .map(|r| {
                (
                    r.library.name.as_str(),
                    r.latest.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
                )
            })
            .collect();
        // The film not on the device is not listed though it is noted.
        assert_eq!(
            rails,
            vec![
                ("TV Shows", vec!["Show".to_owned()]),
                ("Movies", vec!["The Film".to_owned()]),
                ("Books", vec!["A Book".to_owned()]),
            ]
        );
    }

    #[test]
    fn continue_watching_takes_started_videos_and_drops_finished_ones() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let home = run(lib.home(&info())).unwrap();
        let resume: Vec<_> = home.resume.iter().map(|i| i.id).collect();
        assert_eq!(resume, vec![FILM]);
        assert_eq!(home.resume[0].position_ms, 1_200_000);
    }

    #[test]
    fn next_up_is_the_downloaded_episode_after_the_last_watched() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let home = run(lib.home(&info())).unwrap();
        let next: Vec<_> = home.next_up.iter().map(|i| i.id).collect();
        assert_eq!(next, vec![E2]);
    }

    #[test]
    fn books_part_read_are_in_reading_with_their_place() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let home = run(lib.home(&info())).unwrap();
        assert_eq!(
            home.reading.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![BOOK]
        );
        let detail = run(lib.detail(&info(), BOOK)).unwrap();
        assert_eq!(detail.read_position.map(|p| p.fraction), Some(0.3));
    }

    #[test]
    fn a_season_lists_only_its_downloaded_episodes_in_order() {
        let device = vec![
            video(E3, ItemKind::Episode, 0),
            video(E1, ItemKind::Episode, 0),
        ];
        let lib = OfflineLibrary::new(&catalogue(), &device);
        let page = run(lib.query(
            &info(),
            &Listing::of(S1, None, "ParentIndexNumber,IndexNumber"),
            0,
            50,
        ))
        .unwrap();
        assert_eq!(
            page.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![E1, E3]
        );
        assert_eq!(page.total, 2);
    }

    #[test]
    fn a_grouped_listing_finds_series_under_a_library() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let page = run(lib.query(
            &info(),
            &Listing::of(TV, Some(ItemKind::Series), "SortName"),
            0,
            50,
        ))
        .unwrap();
        assert_eq!(
            page.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![SHOW]
        );
    }

    #[test]
    fn views_lists_only_libraries_with_something_on_the_device() {
        let device = vec![video(FILM, ItemKind::Movie, 0)];
        let lib = OfflineLibrary::new(&catalogue(), &device);
        let views = run(lib.views(&info())).unwrap();
        assert_eq!(
            views.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![FILMS]
        );
    }

    #[test]
    fn detail_carries_the_noted_page_text() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let detail = run(lib.detail(&info(), FILM)).unwrap();
        assert_eq!(detail.overview, "A film.");
        assert_eq!(detail.item.year, Some(1994));
    }

    #[test]
    fn an_item_not_on_the_device_is_not_found() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let err = run(lib.detail(&info(), OTHER_FILM)).unwrap_err();
        assert!(
            matches!(
                err,
                ClientError::Api {
                    kind: "notFound",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// Kept since the last notes pass: it shows by its kept title, in a
    /// library of its kind (the noted one when there is one), so it can be
    /// found offline at all.
    #[test]
    fn an_unnoted_download_still_shows_by_its_kept_title() {
        let unnoted = ItemRef::Real(999);
        let lib = OfflineLibrary::new(&Catalogue::new("u"), &[video(unnoted, ItemKind::Movie, 5)]);
        let home = run(lib.home(&info())).unwrap();
        assert_eq!(home.rails.len(), 1);
        assert_eq!(home.rails[0].library.name, "Movies");
        assert_eq!(home.rails[0].latest[0].name, "kept 999");
        assert_eq!(home.resume[0].name, "kept 999");
        let found = run(lib.search(&info(), "KEPT", 0)).unwrap();
        assert_eq!(found.total, 1);

        // An unnoted book still opens: its page knows its format, so it
        // offers Read.
        let book = ItemRef::Real(998);
        let lib = OfflineLibrary::new(
            &Catalogue::new("u"),
            &[OnDevice::Book {
                id: book,
                title: "kept book".into(),
                format: BookFormat::Pdf,
                fraction: 0.0,
            }],
        );
        let detail = run(lib.detail(&info(), book)).unwrap();
        assert_eq!(detail.book.map(|b| b.format), Some(BookFormat::Pdf));

        // With a noted Movies library, it joins that one: one Movies tab.
        let mut device = device();
        device.push(video(unnoted, ItemKind::Movie, 0));
        let lib = OfflineLibrary::new(&catalogue(), &device);
        let views = run(lib.views(&info())).unwrap();
        let movies: Vec<_> = views.items.iter().filter(|l| l.name == "Movies").collect();
        assert_eq!(movies.len(), 1);
        let listed = run(lib.query(&info(), &Listing::of(FILMS, None, "SortName"), 0, 50)).unwrap();
        assert!(listed.items.iter().any(|i| i.id == unnoted), "{listed:?}");
    }

    #[test]
    fn search_matches_names_series_and_artists_on_the_device_only() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let found = run(lib.search(&info(), "film", 0)).unwrap();
        assert_eq!(
            found.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![FILM]
        );
        let by_series = run(lib.search(&info(), "show", 0)).unwrap();
        assert!(by_series.items.iter().any(|i| i.id == E2), "{by_series:?}");
    }

    #[test]
    fn server_only_calls_answer_offline() {
        let lib = OfflineLibrary::new(&catalogue(), &device());
        let err = run(lib.person(&info(), FILM)).unwrap_err();
        assert!(
            matches!(
                err,
                ClientError::Api {
                    kind: OFFLINE_KIND,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn the_index_gives_each_letter_its_first_offset() {
        let device = vec![
            video(FILM, ItemKind::Movie, 0),
            video(OTHER_FILM, ItemKind::Movie, 0),
        ];
        let lib = OfflineLibrary::new(&catalogue(), &device);
        let index = run(lib.index(&info(), &Listing::of(FILMS, None, "SortName"))).unwrap();
        // "The Film" sorts under F.
        assert_eq!(
            index.letters,
            vec![("A".to_owned(), 0), ("F".to_owned(), 1)]
        );
    }

    /// The letters index the listing as shown: with a genre picked, a jump
    /// lands in the genre's listing, and the picker still offers every
    /// genre.
    #[test]
    fn the_index_follows_the_genre_listing_and_offers_every_genre() {
        let films = library(FILMS, LibraryKind::Movies, "Movies");
        let alpha = under(OTHER_FILM, ItemKind::Movie, "Alpha", FILMS);
        let bravo = under(FILM, ItemKind::Movie, "Bravo", FILMS);
        let mut alpha_page = detail_with(&alpha, "");
        alpha_page.genres = vec!["Drama".into()];
        let mut bravo_page = detail_with(&bravo, "");
        bravo_page.genres = vec!["Action".into()];
        let mut c = Catalogue::new("u");
        c.note(&[films.clone(), alpha], Some(&alpha_page));
        c.note(&[films, bravo], Some(&bravo_page));
        let device = vec![
            video(FILM, ItemKind::Movie, 0),
            video(OTHER_FILM, ItemKind::Movie, 0),
        ];
        let lib = OfflineLibrary::new(&c, &device);
        let action = Listing {
            genre: Some("Action".into()),
            ..Listing::of(FILMS, None, "SortName")
        };
        let index = run(lib.index(&info(), &action)).unwrap();
        assert_eq!(index.letters, vec![("B".to_owned(), 0)]);
        assert_eq!(index.genres, vec!["Action".to_owned(), "Drama".to_owned()]);
    }

    /// A kept episode with no note sits straight under its library; the
    /// library's Series listing still lists it, beside the noted series.
    #[test]
    fn a_grouped_listing_keeps_an_unnoted_item_under_its_library() {
        let unnoted = ItemRef::Real(997);
        let mut device = device();
        device.push(video(unnoted, ItemKind::Episode, 0));
        let lib = OfflineLibrary::new(&catalogue(), &device);
        let page = run(lib.query(
            &info(),
            &Listing::of(TV, Some(ItemKind::Series), "SortName"),
            0,
            50,
        ))
        .unwrap();
        let ids: Vec<_> = page.items.iter().map(|i| i.id).collect();
        assert!(ids.contains(&SHOW), "{ids:?}");
        assert!(ids.contains(&unnoted), "{ids:?}");
        assert_eq!(ids.len(), 2, "{ids:?}");
    }

    #[test]
    fn the_catalogue_round_trips_and_is_kept_per_user() {
        let c = catalogue();
        let back = Catalogue::parse(Some(&c.to_json()));
        assert_eq!(back, c);
        assert!(back.clone().for_user("someone else").snaps.is_empty());
        assert_eq!(back.for_user("u"), c);
        assert_eq!(Catalogue::parse(Some("not json")), Catalogue::default());
    }

    #[test]
    fn missing_lists_kept_items_without_a_note() {
        let c = catalogue();
        assert_eq!(
            c.missing(&[FILM, ItemRef::Real(999)]),
            vec![ItemRef::Real(999)]
        );
    }

    #[test]
    fn prune_keeps_kept_items_and_what_leads_to_them() {
        let mut c = catalogue();
        let dropped = c.prune(&[E2]);
        let left: BTreeSet<_> = c.snaps.keys().cloned().collect();
        let want: BTreeSet<_> = [E2, S1, SHOW, TV].iter().map(ToString::to_string).collect();
        assert_eq!(left, want);
        assert_eq!(dropped, 7);
    }

    #[test]
    fn noting_an_ancestor_again_keeps_its_own_page_text() {
        let mut c = Catalogue::new("u");
        let films = library(FILMS, LibraryKind::Movies, "Movies");
        let film = under(FILM, ItemKind::Movie, "The Film", FILMS);
        assert!(c.note(
            &[films.clone(), film.clone()],
            Some(&detail_with(&film, "A film."))
        ));
        assert!(!c.note(
            &[films.clone(), film.clone()],
            Some(&detail_with(&film, "A film."))
        ));
        // A later trail through the film (as an ancestor) does not wipe it.
        let extra = under(ItemRef::Real(999), ItemKind::Other, "Extra", FILM);
        c.note(&[films, film, extra], None);
        assert_eq!(
            c.snaps.get("201").map(|s| s.overview.as_str()),
            Some("A film.")
        );
    }
}
