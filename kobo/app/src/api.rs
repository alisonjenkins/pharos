//! The pharos REST calls this app makes, as plain values: URLs, bodies and
//! parsed replies. Nothing here touches the runtime, so all of it is tested
//! on the host.
//!
//! The Jellyfin-compatible API is used because a Cobalt app reaches the
//! network only through the runtime's HTTPS tasks (ADR-0136).

use kobo_json::{ObjectBuilder, Value};
use kobo_sdk::TaskError;

/// The device id pharos keys this reader's session by.
pub const DEVICE_ID: &str = "pharos-kobo";
/// How many rows one request asks for. One panel page shows fewer, and the
/// rest are turned to without another round trip.
pub const PAGE: u32 = 50;

/// Ticks are 100 ns: what Jellyfin positions are counted in.
const TICKS_PER_MS: u64 = 10_000;

/// A signed-in account: what the app keeps across restarts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    pub user_id: String,
    pub token: String,
}

impl Account {
    /// Two lines: user id, then token. Neither can hold a newline.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        format!("{}\n{}", self.user_id, self.token).into_bytes()
    }

    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (user_id, token) = text.split_once('\n')?;
        (!user_id.is_empty() && !token.is_empty() && !token.contains('\n')).then(|| Self {
            user_id: user_id.to_owned(),
            token: token.to_owned(),
        })
    }
}

/// One row of a listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    /// Artist for an album or a track; empty when pharos sent none.
    pub by: String,
    /// Length in milliseconds, when pharos sent one.
    pub runtime_ms: Option<u64>,
    /// Release year, when pharos sent one.
    pub year: Option<u32>,
    /// Whether pharos marks the album as an audiobook (ADR-0132).
    pub audiobook: bool,
    /// Pharos's tag for the cover, present only when there is a cover to
    /// fetch.
    pub image_tag: Option<String>,
    /// The description, as plain text; empty when pharos sent none, as it
    /// does for music albums and tracks.
    pub overview: String,
}

impl crate::shelf::Item for Entry {
    fn id(&self) -> &str {
        &self.id
    }

    fn image_tag(&self) -> Option<&str> {
        self.image_tag.as_deref()
    }

    fn sort_name(&self) -> &str {
        &self.name
    }
}

/// A page of a listing and how many there are in all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub total: usize,
    /// How many rows the server sent, before any were dropped for lacking an
    /// id. The next request starts this far on.
    pub sent: usize,
}

/// A kind of library the home screen offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Music,
    Audiobooks,
    Books,
}

impl Kind {
    /// The name on the home screen and in the top bar.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Music => "Music",
            Self::Audiobooks => "Audiobooks",
            Self::Books => "Books",
        }
    }

    /// What one entry of the kind is called, for a count.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Music => "albums",
            Self::Audiobooks => "audiobooks",
            Self::Books => "books",
        }
    }

    /// Whether the kind is a list of albums that can be opened and played.
    #[must_use]
    pub fn is_albums(self) -> bool {
        !matches!(self, Self::Books)
    }

    /// Whether covers of this kind are taller than wide: a book's, not a
    /// record's.
    #[must_use]
    pub fn portrait(self) -> bool {
        matches!(self, Self::Audiobooks | Self::Books)
    }

    /// Whether an album belongs under this kind. Pharos lists audiobooks
    /// among albums, told apart only by their mark.
    #[must_use]
    pub fn keeps(self, entry: &Entry) -> bool {
        match self {
            Self::Music => !entry.audiobook,
            Self::Audiobooks => entry.audiobook,
            Self::Books => false,
        }
    }
}

/// One thing the home screen offers: a kind, and the library to list it from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Section {
    pub kind: Kind,
    pub library: String,
}

/// The value of `X-Emby-Authorization`: who is asking, and the token once
/// there is one. `Authorization` itself is the runtime's to set.
#[must_use]
pub fn client_header_value(version: &str, token: Option<&str>) -> String {
    let mut value = format!(
        "MediaBrowser Client=\"Pharos Kobo\", Device=\"Kobo\", DeviceId=\"{DEVICE_ID}\", \
         Version=\"{version}\""
    );
    if let Some(token) = token {
        value.push_str(&format!(", Token=\"{token}\""));
    }
    value
}

/// Keeps a value safe inside a URL query. Ids and secrets are hex already;
/// this is for the day one is not.
#[must_use]
pub fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[must_use]
pub fn initiate_url(server: &str) -> String {
    format!("{server}/QuickConnect/Initiate")
}

#[must_use]
pub fn connect_url(server: &str, secret: &str) -> String {
    format!(
        "{server}/QuickConnect/Connect?secret={}",
        query_escape(secret)
    )
}

#[must_use]
pub fn finish_url(server: &str) -> String {
    format!("{server}/Users/AuthenticateWithQuickConnect")
}

#[must_use]
pub fn finish_body(secret: &str) -> String {
    ObjectBuilder::new().set("Secret", secret).build().to_json()
}

/// The libraries the user can see.
#[must_use]
pub fn views_url(server: &str, account: &Account) -> String {
    format!("{server}/Users/{}/Views", query_escape(&account.user_id))
}

/// The albums under one library, by name. Audiobooks are albums too, so a
/// server that ignores the library sends them with the music; `Kind::keeps`
/// sorts them out. `Overview` carries an audiobook's description.
#[must_use]
pub fn albums_url(server: &str, account: &Account, library: &str, start: usize) -> String {
    format!(
        "{server}/Users/{}/Items?IncludeItemTypes=MusicAlbum&Recursive=true\
         &ParentId={}&SortBy=SortName&SortOrder=Ascending&Fields=Overview\
         &StartIndex={start}&Limit={PAGE}",
        query_escape(&account.user_id),
        query_escape(library)
    )
}

/// One album's tracks, in disc and track order.
#[must_use]
pub fn tracks_url(server: &str, account: &Account, album: &str, start: usize) -> String {
    format!(
        "{server}/Users/{}/Items?ParentId={}&IncludeItemTypes=Audio\
         &SortBy=ParentIndexNumber,IndexNumber,SortName&StartIndex={start}&Limit={PAGE}",
        query_escape(&account.user_id),
        query_escape(album)
    )
}

/// Every codec the reader's decoder plays, as pharos's probe names them.
/// A source in one of these is sent as stored, which pharos serves in byte
/// ranges; anything else comes back as a live MP3 encode, which has no
/// ranges and so must fit in one download (B1449).
pub const DECODES: &str = "mp3,aac,flac,opus,vorbis,pcm_s16le";

/// The track as the reader can decode it. The runtime fetches streams
/// without credentials, so the token rides in the query (ADR-0136).
#[must_use]
pub fn stream_url(server: &str, account: &Account, track: &str) -> String {
    format!(
        "{server}/Audio/{}/universal?AudioCodec={DECODES}&api_key={}",
        query_escape(track),
        query_escape(&account.token)
    )
}

/// How good a JPEG cover pharos is asked for. A cover is dithered to a few
/// greys on the panel, so more than this buys nothing.
const COVER_QUALITY: u8 = 80;
/// The widest cover ever asked for, whatever the panel claims, so a bad
/// measurement cannot ask for a poster.
const MAX_COVER_SIDE: u32 = 640;

/// A cover, sized to fill `width` by `height` pixels. The tag lets pharos
/// answer a repeat from its cache; it is left out when the album has none.
#[must_use]
pub fn image_url(server: &str, id: &str, tag: Option<&str>, width: u32, height: u32) -> String {
    let mut url = format!(
        "{server}/Items/{}/Images/Primary?fillWidth={width}&fillHeight={height}\
         &quality={COVER_QUALITY}&format=jpg",
        query_escape(id)
    );
    if let Some(tag) = tag {
        url.push_str(&format!("&tag={}", query_escape(tag)));
    }
    url
}

/// The pixel size of a cover for a grid of `columns` across a panel
/// `panel_width` wide, with `margin` at each edge and `gutter` between
/// columns. A portrait cell is half as tall again as it is wide, as the
/// layout draws it. Never zero, never wider than [`MAX_COVER_SIDE`].
#[must_use]
pub fn cover_px(
    panel_width: i32,
    margin: i32,
    gutter: i32,
    columns: usize,
    portrait: bool,
) -> (u32, u32) {
    let columns = i32::try_from(columns.max(1)).unwrap_or(1);
    let room = panel_width
        .saturating_sub(margin.saturating_mul(2))
        .saturating_sub(gutter.saturating_mul(columns - 1));
    let cell = u32::try_from(room / columns)
        .unwrap_or(0)
        .clamp(1, MAX_COVER_SIDE);
    let height = if portrait { cell * 3 / 2 } else { cell };
    (cell, height.max(1))
}

/// The grid positions whose covers are wanted when `page` is showing: that
/// page first, then the next one, so turning to it finds the covers ready.
#[must_use]
pub fn wanted(pages: &[Vec<usize>], page: usize) -> Vec<usize> {
    pages
        .iter()
        .skip(page)
        .take(2)
        .flat_map(|indices| indices.iter().copied())
        .collect()
}

/// Where a cover stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cover {
    /// Asked for, not yet here.
    Loading,
    /// Held by the runtime under `handle`, at this size.
    Ready {
        handle: u32,
        width: u32,
        height: u32,
    },
    /// Could not be had, and nothing asks again.
    Failed,
    /// Could not be had for a reason that may pass (the server restarting, a
    /// radio coming back): asked for again once the cache's clock reaches
    /// `due`, in seconds.
    Waiting { due: u64 },
}

/// The covers the runtime is holding on this app's behalf, and the covers
/// failed or in flight, oldest first. The runtime keeps every picture until
/// told to let it go, so the count is bounded here: adding past the capacity
/// pushes out the oldest and says which handles to drop. Pixels are never
/// kept; the runtime has them.
#[derive(Clone, Debug)]
pub struct CoverCache {
    slots: Vec<(String, Cover)>,
    capacity: usize,
    next_handle: u32,
    pinned: Vec<String>,
    /// Seconds the retry sleeps have counted: the app has no clock, so the
    /// time a waiting cover is due is read against this.
    clock: u64,
    /// How many times each cover has failed for a transient reason, so the
    /// wait doubles. Forgotten when the cover arrives, is let go, or is
    /// pushed out.
    attempts: Vec<(String, u32)>,
}

/// The first wait before a cover that failed for a transient reason is asked
/// for again, in seconds. It doubles each time, up to [`RETRY_MAX_SECONDS`].
pub const RETRY_FIRST_SECONDS: u32 = 15;
/// The longest wait between two asks for the same cover, in seconds.
pub const RETRY_MAX_SECONDS: u32 = 300;

/// Whether a cover that failed this way may arrive if asked for again: the
/// radio or the server was not there, or the server asked for patience. A
/// missing picture, a refusal and an oversized one will not change.
#[must_use]
pub const fn transient(error: TaskError) -> bool {
    error.worth_retrying() || matches!(error, TaskError::RateLimited(_))
}

impl CoverCache {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: Vec::new(),
            capacity: capacity.max(1),
            next_handle: 0,
            pinned: Vec::new(),
            clock: 0,
            attempts: Vec::new(),
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
    }

    /// Covers that must stay while something still shows them: the open
    /// album, and the one the player holds.
    pub fn set_pinned(&mut self, ids: Vec<String>) {
        self.pinned = ids;
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn state(&self, id: &str) -> Option<Cover> {
        self.slots
            .iter()
            .find(|(held, _)| held == id)
            .map(|(_, cover)| *cover)
    }

    /// Marks `id` as the most recently wanted, so it goes last.
    pub fn touch(&mut self, id: &str) {
        if let Some(at) = self.slots.iter().position(|(held, _)| held == id) {
            let slot = self.slots.remove(at);
            self.slots.push(slot);
        }
    }

    /// Notes that `id` has been asked for. Returns the handles of the covers
    /// pushed out to make room, for the caller to drop.
    pub fn begin(&mut self, id: &str) -> Vec<u32> {
        if self.state(id).is_some() {
            return Vec::new();
        }
        self.slots.push((id.to_owned(), Cover::Loading));
        let mut dropped = Vec::new();
        while self.slots.len() > self.capacity {
            let Some(at) = self
                .slots
                .iter()
                .position(|(held, _)| held != id && !self.pinned.contains(held))
            else {
                break;
            };
            let (_, cover) = self.slots.remove(at);
            if let Cover::Ready { handle, .. } = cover {
                dropped.push(handle);
            }
        }
        let slots = &self.slots;
        self.attempts
            .retain(|(held, _)| slots.iter().any(|(slot, _)| slot == held));
        dropped
    }

    /// Records that the cover for `id` arrived at this size, and returns the
    /// handle to put it under. `None` when `id` is no longer being waited
    /// for: its reply is stale.
    pub fn ready(&mut self, id: &str, width: u32, height: u32) -> Option<u32> {
        let at = self
            .slots
            .iter()
            .position(|(held, cover)| held == id && *cover == Cover::Loading)?;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        let handle = self.next_handle;
        self.attempts.retain(|(held, _)| held != id);
        if let Some(slot) = self.slots.get_mut(at) {
            slot.1 = Cover::Ready {
                handle,
                width,
                height,
            };
        }
        Some(handle)
    }

    /// Records that `id` could not be had. True the first time, so the
    /// caller logs it once.
    pub fn failed(&mut self, id: &str) -> bool {
        match self.slots.iter_mut().find(|(held, _)| held == id) {
            Some((_, cover)) if !matches!(cover, Cover::Failed) => {
                *cover = Cover::Failed;
                true
            }
            _ => false,
        }
    }

    /// Records that `id`, being waited for, failed for a transient reason:
    /// it is asked for again after the returned seconds, which double with
    /// each failure up to [`RETRY_MAX_SECONDS`] and are at least `at_least`
    /// (a host's own request for patience). `None` when `id` is not being
    /// waited for.
    pub fn retry_later(&mut self, id: &str, at_least: u32) -> Option<u32> {
        let slot = self
            .slots
            .iter_mut()
            .find(|(held, cover)| held == id && *cover == Cover::Loading)?;
        let tried = if let Some((_, tried)) = self.attempts.iter_mut().find(|(held, _)| held == id)
        {
            *tried = tried.saturating_add(1);
            *tried
        } else {
            self.attempts.push((id.to_owned(), 0));
            0
        };
        let wait = RETRY_FIRST_SECONDS
            .saturating_mul(1u32.checked_shl(tried).unwrap_or(u32::MAX))
            .max(at_least)
            .min(RETRY_MAX_SECONDS);
        slot.1 = Cover::Waiting {
            due: self.clock.saturating_add(u64::from(wait)),
        };
        Some(wait)
    }

    /// Lets `seconds` pass on the retry clock.
    pub fn advance(&mut self, seconds: u32) {
        self.clock = self.clock.saturating_add(u64::from(seconds));
    }

    /// Whether `id` waits to be asked for again and its time has come.
    #[must_use]
    pub fn retry_due(&self, id: &str) -> bool {
        matches!(self.state(id), Some(Cover::Waiting { due }) if due <= self.clock)
    }

    /// Notes that `id`, whose retry is due, has been asked for again.
    pub fn retried(&mut self, id: &str) {
        if self.retry_due(id) {
            if let Some(slot) = self.slots.iter_mut().find(|(held, _)| held == id) {
                slot.1 = Cover::Loading;
            }
        }
    }

    /// Seconds until the soonest of `ids` that waits to retry is due, at
    /// least one; `None` when none of them waits.
    #[must_use]
    pub fn next_retry<'a>(&self, ids: impl Iterator<Item = &'a str>) -> Option<u32> {
        ids.filter_map(|id| match self.state(id) {
            Some(Cover::Waiting { due }) => Some(due.saturating_sub(self.clock)),
            _ => None,
        })
        .min()
        .map(|seconds| u32::try_from(seconds).unwrap_or(u32::MAX).max(1))
    }

    /// Forgets a cover still being waited for, because it is wanted no more.
    pub fn release(&mut self, id: &str) {
        self.slots
            .retain(|(held, cover)| held != id || *cover != Cover::Loading);
        let slots = &self.slots;
        self.attempts
            .retain(|(held, _)| slots.iter().any(|(slot, _)| slot == held));
    }

    /// Forgets everything, returning the handles to drop.
    pub fn clear(&mut self) -> Vec<u32> {
        let handles = self
            .slots
            .drain(..)
            .filter_map(|(_, cover)| match cover {
                Cover::Ready { handle, .. } => Some(handle),
                _ => None,
            })
            .collect();
        self.pinned.clear();
        self.attempts.clear();
        handles
    }
}

/// Which playing-session call a report is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Report {
    Started,
    Progress,
    Stopped,
}

impl Report {
    #[must_use]
    pub fn url(self, server: &str) -> String {
        let tail = match self {
            Self::Started => "",
            Self::Progress => "/Progress",
            Self::Stopped => "/Stopped",
        };
        format!("{server}/Sessions/Playing{tail}")
    }

    /// A stable name for logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Progress => "progress",
            Self::Stopped => "stopped",
        }
    }
}

#[must_use]
pub fn report_body(track: &str, position_ms: u64, paused: bool) -> String {
    // Built by hand: kobo-json numbers stop at 32 bits, and ten minutes is
    // already six billion ticks.
    let ticks = position_ms.saturating_mul(TICKS_PER_MS);
    let mut item = String::new();
    kobo_json::escape_into(track, &mut item);
    format!(
        "{{\"ItemId\":{item},\"PositionTicks\":{ticks},\"IsPaused\":{paused},\
         \"PlayMethod\":\"Transcode\"}}"
    )
}

fn parse(bytes: &[u8]) -> Option<Value> {
    kobo_json::parse(std::str::from_utf8(bytes).ok()?).ok()
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `(code to show, secret to poll with)` from `QuickConnect/Initiate`.
#[must_use]
pub fn parse_initiate(bytes: &[u8]) -> Option<(String, String)> {
    let reply = parse(bytes)?;
    Some((
        text(&reply, "Code")?.to_owned(),
        text(&reply, "Secret")?.to_owned(),
    ))
}

/// Whether `QuickConnect/Connect` says the code was approved.
#[must_use]
pub fn parse_connected(bytes: &[u8]) -> bool {
    parse(bytes)
        .and_then(|reply| reply.get("Authenticated").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// The account from `AuthenticateWithQuickConnect`.
#[must_use]
pub fn parse_account(bytes: &[u8]) -> Option<Account> {
    let reply = parse(bytes)?;
    let token = text(&reply, "AccessToken")?;
    let user_id = reply.get("User").and_then(|user| text(user, "Id"))?;
    let account = Account {
        user_id: user_id.to_owned(),
        token: token.to_owned(),
    };
    // Round-trips through the store, or it is not an account worth keeping.
    Account::decode(&account.encode())
}

/// The sections the home screen shows, from `Users/{id}/Views`: music, then
/// audiobooks, then books. A Books library holds both audiobooks and ebooks
/// (ADR-0132); a library with no kind is mixed and listed as music. Videos
/// and the like are left out. Where several libraries share a kind, the
/// first one answers for it.
#[must_use]
pub fn parse_views(bytes: &[u8]) -> Option<Vec<Section>> {
    let reply = parse(bytes)?;
    let items = reply.get("Items").and_then(Value::as_array)?;
    let mut sections: Vec<Section> = Vec::new();
    let mut add = |kind: Kind, library: &str| {
        if !sections.iter().any(|section| section.kind == kind) {
            sections.push(Section {
                kind,
                library: library.to_owned(),
            });
        }
    };
    for item in items {
        let Some(id) = text(item, "Id") else {
            continue;
        };
        match text(item, "CollectionType")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("music" | "mixed") | None => add(Kind::Music, id),
            Some("books") => {
                add(Kind::Audiobooks, id);
                add(Kind::Books, id);
            }
            Some(_) => {}
        }
    }
    sections.sort_by_key(|section| match section.kind {
        Kind::Music => 0,
        Kind::Audiobooks => 1,
        Kind::Books => 2,
    });
    Some(sections)
}

/// One row of `Items`, or `None` when it has no id. A missing name reads as
/// "Untitled" so a row is never blank.
fn entry_of(item: &Value) -> Option<Entry> {
    let by = text(item, "AlbumArtist")
        .or_else(|| {
            item.get("Artists")
                .and_then(Value::as_array)
                .and_then(|artists| artists.first())
                .and_then(Value::as_str)
        })
        .unwrap_or_default();
    let runtime_ms = item
        .get("RunTimeTicks")
        .and_then(Value::as_i64)
        .and_then(|ticks| u64::try_from(ticks).ok())
        .map(|ticks| ticks / TICKS_PER_MS);
    Some(Entry {
        id: text(item, "Id")?.to_owned(),
        name: text(item, "Name").unwrap_or("Untitled").to_owned(),
        by: by.to_owned(),
        runtime_ms,
        year: item
            .get("ProductionYear")
            .and_then(Value::as_i64)
            .and_then(|year| u32::try_from(year).ok())
            .filter(|year| *year > 0),
        audiobook: item
            .get("PharosAudiobook")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        image_tag: item
            .get("ImageTags")
            .and_then(|tags| text(tags, "Primary"))
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned),
        overview: text(item, "Overview").unwrap_or_default().trim().to_owned(),
    })
}

/// A page of `Items`. Rows without an id are dropped.
#[must_use]
pub fn parse_page(bytes: &[u8]) -> Option<Page> {
    let reply = parse(bytes)?;
    let items = reply.get("Items").and_then(Value::as_array)?;
    let entries: Vec<Entry> = items.iter().filter_map(entry_of).collect();
    let total = reply
        .get("TotalRecordCount")
        .and_then(Value::as_i64)
        .and_then(|total| usize::try_from(total).ok())
        .unwrap_or(entries.len());
    Some(Page {
        entries,
        total,
        sent: items.len(),
    })
}

/// One item, from `Users/{id}/Items/{item}`.
#[must_use]
pub fn parse_item(bytes: &[u8]) -> Option<Entry> {
    entry_of(&parse(bytes)?)
}

/// How many part-played tracks the home screen asks for. Several tracks of
/// one album fold into one tile, so this is more than a row of tiles.
pub const RESUME_LIMIT: u32 = 24;
/// The most albums kept from them.
pub const MAX_RESUMED: usize = 8;

/// The tracks the user stopped part-way through, newest first (V1445):
/// what the home screen's Continue listening row is made from.
#[must_use]
pub fn resume_audio_url(server: &str, account: &Account) -> String {
    format!(
        "{server}/Users/{}/Items/Resume?MediaTypes=Audio&Limit={RESUME_LIMIT}",
        query_escape(&account.user_id)
    )
}

/// One item, as the user sees it. An album's id is pharos's own, which the
/// item route answers like any other.
#[must_use]
pub fn item_url(server: &str, account: &Account, item: &str) -> String {
    format!(
        "{server}/Users/{}/Items/{}",
        query_escape(&account.user_id),
        query_escape(item)
    )
}

/// An album the user stopped part-way through, and where.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resumed {
    /// The album, first as the track names it, then as pharos describes it
    /// once [`Self::described`].
    pub album: Entry,
    /// Whether `album` is pharos's own description, with its cover and its
    /// audiobook mark, rather than what the track said.
    pub described: bool,
    /// The track to carry on from.
    pub track: String,
    pub position_ms: u64,
}

/// The part-played tracks from `Items/Resume`, folded to one per album and
/// kept in the server's order, newest first. A track with no album, or no
/// place to carry on from, is left out.
#[must_use]
pub fn parse_resumed(bytes: &[u8]) -> Option<Vec<Resumed>> {
    let reply = parse(bytes)?;
    let items = reply.get("Items").and_then(Value::as_array)?;
    let mut resumed: Vec<Resumed> = Vec::new();
    for item in items {
        let (Some(track), Some(album)) = (text(item, "Id"), text(item, "AlbumId")) else {
            continue;
        };
        let position_ms = item
            .get("UserData")
            .and_then(|data| data.get("PlaybackPositionTicks"))
            .and_then(Value::as_i64)
            .and_then(|ticks| u64::try_from(ticks).ok())
            .map_or(0, |ticks| ticks / TICKS_PER_MS);
        if position_ms == 0 || resumed.iter().any(|known| known.album.id == album) {
            continue;
        }
        resumed.push(Resumed {
            album: Entry {
                id: album.to_owned(),
                name: text(item, "Album").unwrap_or("Untitled").to_owned(),
                by: text(item, "AlbumArtist").unwrap_or_default().to_owned(),
                runtime_ms: None,
                year: None,
                audiobook: false,
                image_tag: None,
                overview: String::new(),
            },
            described: false,
            track: track.to_owned(),
            position_ms,
        });
        if resumed.len() >= MAX_RESUMED {
            break;
        }
    }
    Some(resumed)
}

/// "3:07", "1:02:05": a length as a listener reads it.
#[must_use]
pub fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            user_id: "u1".to_owned(),
            token: "t0k".to_owned(),
        }
    }

    #[test]
    fn an_account_survives_the_store_and_junk_does_not() {
        let account = account();
        assert_eq!(Account::decode(&account.encode()), Some(account));
        assert_eq!(Account::decode(b"only-one-line"), None);
        assert_eq!(Account::decode(b"\ntoken"), None);
        assert_eq!(Account::decode(b"user\n"), None);
        assert_eq!(Account::decode(b"user\ntok\nextra"), None);
    }

    #[test]
    fn the_client_header_names_the_device_and_adds_the_token_once_signed_in() {
        let anonymous = client_header_value("0.1.0", None);
        assert!(anonymous.contains("DeviceId=\"pharos-kobo\""));
        assert!(!anonymous.contains("Token"));
        assert!(client_header_value("0.1.0", Some("abc")).ends_with(", Token=\"abc\""));
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(query_escape("ab-12_.~"), "ab-12_.~");
        assert_eq!(query_escape("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn the_stream_asks_for_every_codec_the_reader_decodes_and_carries_the_token() {
        let url = stream_url("https://p.test", &account(), "id9");
        assert_eq!(
            url,
            "https://p.test/Audio/id9/universal\
             ?AudioCodec=mp3,aac,flac,opus,vorbis,pcm_s16le&api_key=t0k"
        );
    }

    #[test]
    fn an_audiobook_codec_is_taken_as_stored() {
        // An M4B is AAC: asking for MP3 alone made pharos encode it live,
        // and a live encode cannot be fetched in ranges (B1449).
        assert!(DECODES.split(',').any(|codec| codec == "aac"));
    }

    #[test]
    fn listings_page_and_sort() {
        let albums = albums_url("https://p.test", &account(), "lib1", 50);
        assert!(albums.starts_with("https://p.test/Users/u1/Items?IncludeItemTypes=MusicAlbum"));
        assert!(albums.contains("&ParentId=lib1&"));
        assert!(albums.contains("&Fields=Overview&"));
        assert!(albums.contains("StartIndex=50&Limit=50"));
        let tracks = tracks_url("https://p.test", &account(), "al1", 50);
        assert!(tracks.contains("ParentId=al1"));
        assert!(tracks.contains("StartIndex=50&Limit=50"));
        assert!(tracks.contains("SortBy=ParentIndexNumber,IndexNumber"));
    }

    #[test]
    fn quick_connect_replies_parse() {
        let initiate = br#"{"Code":"123456","Secret":"s3","Authenticated":false}"#;
        assert_eq!(
            parse_initiate(initiate),
            Some(("123456".to_owned(), "s3".to_owned()))
        );
        assert!(!parse_connected(initiate));
        assert!(parse_connected(br#"{"Authenticated":true}"#));
        assert!(!parse_connected(b"not json"));
        let finished = br#"{"AccessToken":"t0k","User":{"Id":"u1","Name":"x"}}"#;
        assert_eq!(parse_account(finished), Some(account()));
        assert_eq!(parse_account(br#"{"AccessToken":"t0k"}"#), None);
    }

    #[test]
    fn a_page_keeps_rows_with_ids_and_reads_lengths_and_artists() {
        let bytes = br#"{"Items":[
            {"Id":"a","Name":"One","AlbumArtist":"Band","RunTimeTicks":1850000000},
            {"Id":"b","Artists":["Solo"]},
            {"Name":"no id"}
        ],"TotalRecordCount":120}"#;
        let page = parse_page(bytes).unwrap_or_default();
        assert_eq!(page.total, 120);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].by, "Band");
        assert_eq!(page.entries[0].runtime_ms, Some(185_000));
        assert_eq!(page.entries[1].name, "Untitled");
        assert_eq!(page.entries[1].by, "Solo");
        assert_eq!(page.sent, 3, "the row without an id was still sent");
    }

    #[test]
    fn the_views_url_names_the_user() {
        assert_eq!(
            views_url("https://p.test", &account()),
            "https://p.test/Users/u1/Views"
        );
    }

    #[test]
    fn views_become_sections_in_a_fixed_order() {
        let bytes = br#"{"Items":[
            {"Id":"v1","Name":"Films","CollectionType":"movies"},
            {"Id":"v2","Name":"Reading","CollectionType":"books"},
            {"Id":"v3","Name":"Songs","CollectionType":"music"},
            {"Id":"v4","Name":"Other Songs","CollectionType":"music"},
            {"Name":"no id","CollectionType":"music"}
        ]}"#;
        let sections = parse_views(bytes).unwrap_or_default();
        let kinds: Vec<Kind> = sections.iter().map(|section| section.kind).collect();
        assert_eq!(kinds, [Kind::Music, Kind::Audiobooks, Kind::Books]);
        assert_eq!(sections[0].library, "v3", "the first music library answers");
        assert_eq!(sections[1].library, "v2");
        assert_eq!(sections[2].library, "v2");
    }

    #[test]
    fn a_library_with_no_kind_is_music_and_videos_are_left_out() {
        let bytes = br#"{"Items":[
            {"Id":"v1","Name":"Everything","CollectionType":null},
            {"Id":"v2","Name":"Shows","CollectionType":"tvshows"}
        ]}"#;
        let sections = parse_views(bytes).unwrap_or_default();
        assert_eq!(
            sections,
            [Section {
                kind: Kind::Music,
                library: "v1".to_owned()
            }]
        );
        assert_eq!(parse_views(br#"{"Items":[]}"#), Some(Vec::new()));
        assert_eq!(parse_views(b"nope"), None);
    }

    #[test]
    fn audiobooks_are_told_from_music_by_their_mark() {
        let bytes = br#"{"Items":[
            {"Id":"a","Name":"Album One","PharosAudiobook":true,"ProductionYear":2001},
            {"Id":"b","Name":"Album Two","ProductionYear":0}
        ],"TotalRecordCount":9}"#;
        let page = parse_page(bytes).unwrap_or_default();
        assert_eq!(page.sent, 2);
        assert_eq!(page.entries[0].year, Some(2001));
        assert_eq!(page.entries[1].year, None);
        assert!(Kind::Audiobooks.keeps(&page.entries[0]));
        assert!(!Kind::Music.keeps(&page.entries[0]));
        assert!(Kind::Music.keeps(&page.entries[1]));
        assert!(!Kind::Books.keeps(&page.entries[1]));
        assert!(Kind::Music.is_albums() && !Kind::Books.is_albums());
    }

    #[test]
    fn an_audiobook_carries_its_description_and_music_none() {
        let bytes = br#"{"Items":[
            {"Id":"a","Name":"Album One","PharosAudiobook":true,"Overview":" Read aloud. "},
            {"Id":"b","Name":"Album Two"}
        ]}"#;
        let page = parse_page(bytes).unwrap_or_default();
        assert_eq!(page.entries[0].overview, "Read aloud.");
        assert_eq!(page.entries[1].overview, "");
    }

    #[test]
    fn a_whole_albums_length_survives_the_json_numbers() {
        // One hour is 3.6e10 ticks, well past 32 bits.
        let page =
            parse_page(br#"{"Items":[{"Id":"a","RunTimeTicks":36000000000}]}"#).unwrap_or_default();
        assert_eq!(page.entries[0].runtime_ms, Some(3_600_000));
    }

    #[test]
    fn a_cover_url_names_its_size_and_tag() {
        assert_eq!(
            image_url("https://p.test", "al1", Some("t9"), 310, 465),
            "https://p.test/Items/al1/Images/Primary?fillWidth=310&fillHeight=465\
             &quality=80&format=jpg&tag=t9"
        );
        assert!(!image_url("https://p.test", "al1", None, 1, 1).contains("tag="));
    }

    #[test]
    fn a_cover_is_sized_to_its_tile() {
        // A Clara: 1072 wide, 3 columns, 47 px margins, 24 px gutters.
        assert_eq!(cover_px(1072, 47, 24, 3, false), (310, 310));
        assert_eq!(cover_px(1072, 47, 24, 3, true), (310, 465));
        // Never empty, never a poster.
        assert_eq!(cover_px(0, 47, 24, 3, false), (1, 1));
        assert_eq!(cover_px(10_000, 0, 0, 1, false), (640, 640));
        assert_eq!(
            cover_px(1072, 47, 24, 0, false).0,
            640,
            "no columns reads as one"
        );
    }

    #[test]
    fn the_covers_wanted_are_the_page_showing_then_the_next() {
        let pages = vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]];
        assert_eq!(wanted(&pages, 0), [0, 1, 2, 3, 4, 5]);
        assert_eq!(wanted(&pages, 1), [3, 4, 5, 6]);
        assert_eq!(wanted(&pages, 2), [6]);
        assert!(wanted(&pages, 3).is_empty());
        assert!(wanted(&[], 0).is_empty());
    }

    #[test]
    fn a_cover_tag_is_read_only_when_there_is_one() {
        let page = parse_page(
            br#"{"Items":[{"Id":"a","ImageTags":{"Primary":"t1"}},{"Id":"b","ImageTags":{}},{"Id":"c"}]}"#,
        )
        .unwrap_or_default();
        assert_eq!(page.entries[0].image_tag.as_deref(), Some("t1"));
        assert_eq!(page.entries[1].image_tag, None);
        assert_eq!(page.entries[2].image_tag, None);
    }

    fn arrive(cache: &mut CoverCache, id: &str) -> Vec<u32> {
        let dropped = cache.begin(id);
        assert!(cache.ready(id, 10, 10).is_some());
        dropped
    }

    #[test]
    fn the_cover_cache_pushes_out_the_oldest_and_names_its_handles() {
        let mut cache = CoverCache::new(2);
        assert!(arrive(&mut cache, "a").is_empty());
        assert!(arrive(&mut cache, "b").is_empty());
        let first = cache.state("a");
        assert!(matches!(first, Some(Cover::Ready { .. })));
        let dropped = arrive(&mut cache, "c");
        assert_eq!(dropped.len(), 1);
        assert_eq!(cache.state("a"), None);
        assert_eq!(cache.len(), 2);
        if let Some(Cover::Ready { handle, .. }) = first {
            assert_eq!(dropped, [handle], "the handle of the one pushed out");
        }
    }

    #[test]
    fn touching_a_cover_keeps_it_and_pinning_protects_it() {
        let mut cache = CoverCache::new(2);
        arrive(&mut cache, "a");
        arrive(&mut cache, "b");
        cache.touch("a");
        arrive(&mut cache, "c");
        assert!(cache.state("a").is_some() && cache.state("b").is_none());
        cache.set_pinned(vec!["a".to_owned()]);
        arrive(&mut cache, "d");
        assert!(cache.state("a").is_some(), "a pinned cover stays");
        assert!(cache.state("c").is_none());
    }

    #[test]
    fn a_reply_with_nobody_waiting_gets_no_handle() {
        let mut cache = CoverCache::new(2);
        assert_eq!(cache.ready("a", 1, 1), None);
        cache.begin("a");
        cache.release("a");
        assert_eq!(cache.ready("a", 1, 1), None, "released, so stale");
        cache.begin("b");
        assert!(cache.ready("b", 1, 1).is_some());
        assert_eq!(cache.ready("b", 1, 1), None, "only once");
    }

    #[test]
    fn handles_are_never_reused_and_a_failure_is_noted_once() {
        let mut cache = CoverCache::new(1);
        cache.begin("a");
        let first = cache.ready("a", 1, 1);
        cache.begin("b");
        let second = cache.ready("b", 1, 1);
        assert_ne!(first, second);
        assert!(cache.failed("b"));
        assert!(!cache.failed("b"), "logged once");
        assert!(!cache.failed("missing"));
        cache.begin("c");
        assert_eq!(
            cache.clear().len(),
            0,
            "a failed or loading cover holds no handle"
        );
        assert_eq!(cache.state("c"), None);
    }

    #[test]
    fn reports_carry_ticks_and_go_to_the_session_endpoints() {
        let body = report_body("id9", 1_500, true);
        assert!(body.contains("\"PositionTicks\":15000000"));
        assert!(body.contains("\"IsPaused\":true"));
        assert!(body.starts_with("{\"ItemId\":\"id9\","), "{body}");
        assert!(report_body("x", 600_000, false).contains(":6000000000,"));
        assert_eq!(
            Report::Started.url("https://p.test"),
            "https://p.test/Sessions/Playing"
        );
        assert_eq!(
            Report::Stopped.url("https://p.test"),
            "https://p.test/Sessions/Playing/Stopped"
        );
        let labels = [Report::Started, Report::Progress, Report::Stopped].map(Report::label);
        assert_eq!(labels, ["started", "progress", "stopped"]);
    }

    #[test]
    fn lengths_read_as_clocks() {
        assert_eq!(clock(187_000), "3:07");
        assert_eq!(clock(3_725_000), "1:02:05");
        assert_eq!(clock(0), "0:00");
    }

    #[test]
    fn resumed_tracks_fold_to_one_per_album_newest_first() {
        let reply = br#"{"Items":[
            {"Id":"t3","AlbumId":"al2","Album":"Later","AlbumArtist":"B",
             "UserData":{"PlaybackPositionTicks":6000000000}},
            {"Id":"t1","AlbumId":"al1","Album":"Earlier",
             "UserData":{"PlaybackPositionTicks":10000}},
            {"Id":"t2","AlbumId":"al2","UserData":{"PlaybackPositionTicks":5}},
            {"Id":"t4","UserData":{"PlaybackPositionTicks":50000}},
            {"Id":"t5","AlbumId":"al3","UserData":{"PlaybackPositionTicks":0}}
        ]}"#;
        let resumed = parse_resumed(reply).expect("parsed");
        let albums: Vec<(&str, &str, u64)> = resumed
            .iter()
            .map(|r| (r.album.id.as_str(), r.track.as_str(), r.position_ms))
            .collect();
        assert_eq!(albums, vec![("al2", "t3", 600_000), ("al1", "t1", 1)]);
        assert_eq!(resumed[0].album.name, "Later");
        assert_eq!(resumed[0].album.by, "B");
        assert!(!resumed[0].described);
    }

    #[test]
    fn resumed_albums_are_capped() {
        let items: Vec<String> = (0..20)
            .map(|i| {
                format!(r#"{{"Id":"t{i}","AlbumId":"a{i}","UserData":{{"PlaybackPositionTicks":99999}}}}"#)
            })
            .collect();
        let reply = format!(r#"{{"Items":[{}]}}"#, items.join(","));
        assert_eq!(
            parse_resumed(reply.as_bytes()).map(|r| r.len()),
            Some(MAX_RESUMED)
        );
    }

    #[test]
    fn a_single_item_parses_like_a_row() {
        let entry = parse_item(
            br#"{"Id":"al1","Name":"N","PharosAudiobook":true,"ImageTags":{"Primary":"x"}}"#,
        )
        .expect("an item");
        assert!(entry.audiobook);
        assert_eq!(entry.image_tag.as_deref(), Some("x"));
    }

    #[test]
    fn the_resume_url_asks_for_audio_only() {
        let url = resume_audio_url(
            "https://p",
            &Account {
                user_id: "u 1".to_owned(),
                token: "t".to_owned(),
            },
        );
        assert_eq!(
            url,
            format!("https://p/Users/u%201/Items/Resume?MediaTypes=Audio&Limit={RESUME_LIMIT}")
        );
    }
}
