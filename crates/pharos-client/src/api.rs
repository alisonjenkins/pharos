//! The UI's view of the server (009 `PharosApi` port): library and playback
//! as pharos domain calls, with plain Rust types in and out. [`NativeApi`]
//! implements it over a signed-in capnp `Session`; nothing above this module
//! sees a capnp type.

use std::fmt;
use std::str::FromStr;

use pharos_api::pharos_capnp::{
    avatar_result, decision_result, detail_result, done, home_result, index_result, item_id,
    item_query, item_summary, items_result, library, lyrics_result, me_result, media_token_result,
    negotiate_result, opt_u16, opt_u32, playback, playback_grant, pref_result, prefs, session,
    stream_choice, telemetry, track, user_data, user_state_result, Kind,
    LibraryKind as WireLibraryKind, SegmentKind as WireSegmentKind, SubtitleDelivery,
};
pub use pharos_api::pharos_capnp::{EventKind as ClientEventKind, Level as ClientLevel};
pub use pharos_api::PASSWORD_CHANGED_REVOKE_FAILED;

use crate::group::NativeGroup;
use crate::{CallInfo, ClientError};

/// An item's id. Stored items are `real`; series, seasons, albums and
/// artists the server groups are `synth`. In a URL a real id is its number
/// and a synth id is `s` and its number, so `/title/42` and `/title/s7`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemRef {
    Real(u64),
    Synth(u64),
}

impl fmt::Display for ItemRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemRef::Real(n) => write!(f, "{n}"),
            ItemRef::Synth(n) => write!(f, "s{n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an item id: {0:?}")]
pub struct BadItemRef(pub String);

impl FromStr for ItemRef {
    type Err = BadItemRef;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || BadItemRef(s.to_owned());
        // Digits only: `parse` would also take a sign, and one item would
        // then have two URLs.
        let number = |n: &str| match n.bytes().all(|b| b.is_ascii_digit()) {
            true => n.parse::<u64>().map_err(|_| bad()),
            false => Err(bad()),
        };
        match s.strip_prefix('s') {
            Some(n) => number(n).map(ItemRef::Synth),
            None => number(s).map(ItemRef::Real),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Movie,
    Episode,
    Audio,
    Book,
    Series,
    Season,
    Album,
    Artist,
    Person,
    Collection,
    Playlist,
    Channel,
    Library,
    /// A kind added to the schema after this client was built.
    Other,
}

impl ItemKind {
    /// Whether `Playback.negotiate` can play it directly.
    pub fn playable(self) -> bool {
        matches!(self, ItemKind::Movie | ItemKind::Episode | ItemKind::Audio)
    }
}

/// What a library holds, which decides how its page lays it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryKind {
    Mixed,
    Movies,
    TvShows,
    Music,
    Books,
}

/// One item as a list or rail shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: ItemRef,
    pub kind: ItemKind,
    pub name: String,
    pub year: Option<u16>,
    pub runtime_ms: u64,
    /// Same-origin URL of its primary image; the browser's `pharos_media`
    /// cookie authenticates the fetch (ADR-0034).
    pub image: Option<String>,
    pub played: bool,
    pub favorite: bool,
    pub position_ms: u64,
    pub series_name: Option<String>,
    /// An episode's season, or a track's disc.
    pub season: Option<u32>,
    /// An episode's number, or a track's number on its disc.
    pub episode: Option<u32>,
    /// Where the server says: a season's series, an episode's season.
    pub parent: Option<ItemRef>,
    /// A library's kind; `None` for anything else.
    pub library_kind: Option<LibraryKind>,
    /// IMDb score in tenths: 78 is 7.8.
    pub imdb_rating: Option<u16>,
    /// Rotten Tomatoes Tomatometer, a percentage.
    pub critic_rating: Option<u16>,
    /// A song's artists, joined, for its card; `None` for anything else or
    /// from a server before schemaVersion 25.
    pub artist: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeRail {
    pub library: Item,
    pub latest: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Home {
    pub rails: Vec<HomeRail>,
    /// Films and episodes to resume.
    pub resume: Vec<Item>,
    /// Music to resume.
    pub listening: Vec<Item>,
    pub next_up: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    pub items: Vec<Item>,
    pub total: u32,
    pub start: u32,
}

/// One listing of a parent's children: what it holds, its order, and what
/// narrows it (B312/V252). Enough to ask for any page of it again.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Listing {
    pub parent: ItemRef,
    /// A kind the server synthesises (series, season, album, artist) lists
    /// those groups; `None` lists the stored items.
    pub group: Option<ItemKind>,
    /// Jellyfin `SortBy` tokens, such as `SortName` or
    /// `ParentIndexNumber,IndexNumber`.
    pub sort_by: String,
    pub descending: bool,
    /// One genre, by name.
    pub genre: Option<String>,
    /// Only played (`Some(true)`) or unplayed (`Some(false)`) items.
    pub played: Option<bool>,
}

impl Listing {
    /// The whole of `group` under `parent` in `sort_by` order, ascending.
    pub fn of(parent: ItemRef, group: Option<ItemKind>, sort_by: &str) -> Self {
        Self {
            parent,
            group,
            sort_by: sort_by.to_owned(),
            descending: false,
            genre: None,
            played: None,
        }
    }
}

/// Where each section of a listing starts, and the genres it can be narrowed
/// to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListIndex {
    /// Each section the listing holds and the offset of its first item, in
    /// listing order: in name order a first letter (`#` for anything not A to
    /// Z), in a date order a year (`#` for no date).
    pub letters: Vec<(String, u32)>,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    pub item: Item,
    pub overview: String,
    pub genres: Vec<String>,
    pub chapters: Vec<Chapter>,
    pub audio_tracks: Vec<Track>,
    pub subtitle_tracks: Vec<Track>,
    /// One sprite set per width the server renders, narrowest first as the
    /// server lists them.
    pub trickplay: Vec<Trickplay>,
    pub segments: Vec<Segment>,
    /// The fonts the file embeds, for drawing an ASS subtitle (026).
    pub fonts: Vec<Font>,
    /// A song's track artists, in display order; empty for anything else.
    pub artists: Vec<String>,
    /// A song's album name.
    pub album: Option<String>,
}

/// A font the file embeds. `url` holds `{subtitle}`, the index of the
/// subtitle track it is fetched for: the server cuts it to that track's glyphs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Font {
    pub index: u32,
    pub filename: String,
    pub mime: String,
    pub url: String,
}

/// A song's lyrics. When `synced`, every line has the time it is sung from;
/// otherwise the lines are plain text and a blank one parts verses. No lines
/// means the song has none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lyrics {
    pub synced: bool,
    pub lines: Vec<LyricLine>,
    pub words_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub start_ms: u64,
    pub text: String,
    pub words: Vec<LyricWord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricWord {
    pub at: u32,
    pub start_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub name: String,
    pub start_ms: u64,
}

/// An audio or subtitle stream; `index` is what [`TrackChoice::Index`] names.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Track {
    pub index: u32,
    pub language: String,
    pub title: String,
    pub codec: String,
    pub is_default: bool,
    pub is_forced: bool,
    pub is_hearing_impaired: bool,
    /// A text subtitle's same-origin WebVTT, which the player renders
    /// itself. `None` for a subtitle the server must burn in, so choosing it
    /// renegotiates; always `None` for audio.
    pub vtt_url: Option<String>,
    /// An image subtitle's same-origin PNG manifest, which the player draws
    /// itself (spec 019); set only when the server extracts image subtitles.
    pub images_url: Option<String>,
    /// The generated Pharos-styled ASS script (spec 028), fetched with
    /// `?tier=&effects=`; `None` for every other track.
    pub styled_url: Option<String>,
}

/// Trickplay sprites: `tile_url` with `{tile}` replaced fetches tile n, a
/// `grid` × `grid` sheet of thumbnails, one every `interval_ms`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Trickplay {
    pub width: u32,
    pub height: u32,
    pub interval_ms: u32,
    pub thumb_count: u32,
    pub tile_count: u32,
    pub grid: u32,
    pub tile_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Intro,
    Outro,
    Recap,
    Preview,
    Commercial,
}

/// A span the player offers to skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Which track to play: the server's pick, none, or a stream index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackChoice {
    #[default]
    Default,
    Off,
    Index(u32),
}

/// The audio and subtitle a playback asks for, or a report says are playing.
/// `Default` in a report says nothing about that track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrackPicks {
    pub audio: TrackChoice,
    pub subtitle: TrackChoice,
}

/// What the player can decode, for the server to choose a delivery.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Capabilities {
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub containers: Vec<String>,
    /// Bits per second; 0 = no limit.
    pub max_bitrate: u64,
    /// Whether the display shows HDR; false gets HDR sources transcoded.
    pub hdr: bool,
    /// Image subtitle codecs the player draws from the server's PNGs.
    pub image_subtitles: Vec<String>,
}

/// The server's playback decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// `direct_play`, `audio_remux`, `video_remux` or `transcode`.
    pub method: String,
    pub direct_play_block: String,
    pub downgrade: String,
    /// Same-origin URL: a direct stream, or an HLS master playlist.
    pub stream_url: String,
    pub play_session: String,
    /// The audio track the playback starts on.
    pub audio: Option<u32>,
    /// The subtitle it starts with; `None` is off.
    pub subtitle: Option<u32>,
}

impl Decision {
    /// Whether `stream_url` is an HLS playlist rather than a file.
    pub fn is_hls(&self) -> bool {
        let path = self.stream_url.split('?').next().unwrap_or_default();
        path.ends_with(".m3u8")
    }
}

/// The calls the UI makes. Async methods without `Send`: the browser runs
/// everything on one thread.
#[allow(async_fn_in_trait)]
pub trait PharosApi {
    type Playing: PlaybackGrant;

    async fn home(&self, call: &CallInfo) -> Result<Home, ClientError>;
    /// The libraries the user may see, in the server's order.
    async fn views(&self, call: &CallInfo) -> Result<Page, ClientError>;
    /// The children of `parent` (a library, series, season, album), sorted
    /// by name, `limit` from `start`.
    async fn browse(
        &self,
        call: &CallInfo,
        parent: ItemRef,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError> {
        self.list(call, parent, None, "SortName", start, limit)
            .await
    }
    /// The children of `parent` that `group` names: a kind the server
    /// synthesises (series, season, album, artist) lists those groups
    /// folded out of what is stored there; `None` lists the stored items.
    /// `sort_by` takes Jellyfin `SortBy` tokens, such as
    /// `ParentIndexNumber,IndexNumber` for episode or track order.
    async fn list(
        &self,
        call: &CallInfo,
        parent: ItemRef,
        group: Option<ItemKind>,
        sort_by: &str,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError> {
        self.query(call, &Listing::of(parent, group, sort_by), start, limit)
            .await
    }
    /// `limit` items of `listing` from `start`.
    async fn query(
        &self,
        call: &CallInfo,
        listing: &Listing,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError>;
    /// Where each letter starts in `listing`, and its genres.
    async fn index(&self, call: &CallInfo, listing: &Listing) -> Result<ListIndex, ClientError>;
    /// The items whose title or text matches `text`, best match first, at
    /// most `limit` (0 is the server's default page).
    async fn search(&self, call: &CallInfo, text: &str, limit: u32) -> Result<Page, ClientError>;
    async fn detail(&self, call: &CallInfo, id: ItemRef) -> Result<Detail, ClientError>;
    async fn lyrics(
        &self,
        call: &CallInfo,
        id: ItemRef,
        want_words: bool,
    ) -> Result<Lyrics, ClientError>;
    /// The next `count` songs for `spec`, given what was heard or queued
    /// (`recent`, oldest first). Short with `exhausted` when it runs out.
    async fn mix(
        &self,
        call: &CallInfo,
        spec: &crate::mix::MixSpec,
        recent: &[ItemRef],
        count: u32,
    ) -> Result<crate::mix::MixBatch, ClientError>;
    /// Negotiate playback of `id` with the tracks in `picks`.
    async fn play(
        &self,
        call: &CallInfo,
        id: ItemRef,
        caps: &Capabilities,
        picks: TrackPicks,
    ) -> Result<(Decision, Self::Playing), ClientError>;
}

/// One negotiated playback, for progress reports.
#[allow(async_fn_in_trait)]
pub trait PlaybackGrant {
    /// `playing` names a track the player switched to without
    /// renegotiating, so the server remembers it.
    async fn report(
        &self,
        call: &CallInfo,
        position_ms: u64,
        paused: bool,
        playing: TrackPicks,
    ) -> Result<(), ClientError>;
    async fn stop(&self, call: &CallInfo, position_ms: u64) -> Result<(), ClientError>;
}

/// A token from [`NativeApi::media_token`].
#[derive(Clone, PartialEq, Eq)]
pub struct MediaToken {
    /// The secret. Never log it.
    pub token: String,
    /// When the token stops working, in ms since the Unix epoch.
    pub expires_at_ms: u64,
}

impl fmt::Debug for MediaToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaToken")
            .field("token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// [`PharosApi`] over a signed-in native session.
#[derive(Clone)]
pub struct NativeApi {
    session: session::Client,
    library: library::Client,
    playback: playback::Client,
    prefs: prefs::Client,
    telemetry: telemetry::Client,
    group: NativeGroup,
    user_data: user_data::Client,
}

/// The signed-in user, as `Session.me` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Me {
    /// The user's id in the simple hex form Jellyfin's routes take.
    pub id: String,
    pub name: String,
    pub is_admin: bool,
    /// Where the user's avatar is, when they have one; the URL carries its
    /// tag, so it changes when the picture does.
    pub avatar_url: Option<String>,
}

/// One client event for `Telemetry.submit`, in plain Rust types.
#[derive(Debug, Clone)]
pub struct ClientEvent {
    pub kind: ClientEventKind,
    pub level: ClientLevel,
    pub route: String,
    pub fields: Vec<(&'static str, String)>,
}

impl NativeApi {
    /// The capabilities are pipelined: no round trip until the first call.
    pub fn new(session: &session::Client) -> Self {
        Self {
            session: session.clone(),
            library: session.library_request().send().pipeline.get_library(),
            playback: session.playback_request().send().pipeline.get_playback(),
            prefs: session.prefs_request().send().pipeline.get_prefs(),
            telemetry: session.telemetry_request().send().pipeline.get_telemetry(),
            group: NativeGroup::new(session),
            user_data: session.user_data_request().send().pipeline.get_user_data(),
        }
    }

    /// Marks `id` watched or not for the signed-in user; a series or season
    /// marks every episode in it. Unwatched also forgets where it was left.
    /// Answers whether it is now watched.
    pub async fn set_played(
        &self,
        call: &CallInfo,
        id: ItemRef,
        played: bool,
    ) -> Result<bool, ClientError> {
        let mut req = self.user_data.set_played_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            set_item_ref(p.reborrow().init_id(), id);
            p.set_played(played);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            user_state_result::State(s) => Ok(s?.get_played()),
            user_state_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Who is signed in.
    pub async fn me(&self, call: &CallInfo) -> Result<Me, ClientError> {
        let mut req = self.session.me_request();
        call.write(req.get().init_ctx());
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            me_result::Me(me) => {
                let me = me?;
                let id: String = me
                    .get_id()?
                    .get_uuid()?
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                let tag = text(me.get_avatar_tag())?;
                let avatar_url =
                    (!tag.is_empty()).then(|| format!("/UserImage?userId={id}&tag={tag}"));
                Ok(Me {
                    id,
                    name: text(me.get_name())?,
                    is_admin: me.get_is_admin(),
                    avatar_url,
                })
            }
            me_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Watch together: the session's `Group`.
    pub fn group(&self) -> &NativeGroup {
        &self.group
    }

    /// Changes the signed-in user's password. The server checks `current`
    /// and signs the user's other sessions out; this one stays signed in.
    pub async fn change_password(
        &self,
        call: &CallInfo,
        current: &str,
        new: &str,
    ) -> Result<(), ClientError> {
        let mut req = self.session.change_password_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_current(current);
            p.set_new(new);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            done::Ok(()) => Ok(()),
            done::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Approves a Quick Connect code shown on another device, which then
    /// signs in as the signed-in user.
    pub async fn quick_connect_authorize(
        &self,
        call: &CallInfo,
        code: &str,
    ) -> Result<(), ClientError> {
        let mut req = self.session.quick_connect_authorize_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_code(code);
        }
        let reply = req.send().promise.await?;
        crate::read_done(reply.get()?.get_result()?)
    }

    /// Sets the signed-in user's avatar from a picture's bytes; answers the
    /// new tag.
    pub async fn set_avatar(&self, call: &CallInfo, image: &[u8]) -> Result<String, ClientError> {
        let mut req = self.session.set_avatar_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_image(image);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            avatar_result::Tag(tag) => Ok(text(tag)?),
            avatar_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// A narrow token for loading images and media in a player that cannot
    /// send the device token or a cookie (ADR-0058): put it in the URL as
    /// `api_key`. It reaches only item images and streams, and stops working
    /// at `expires_at_ms`; ask again before then.
    pub async fn media_token(&self, call: &CallInfo) -> Result<MediaToken, ClientError> {
        let mut req = self.session.media_token_request();
        call.write(req.get().init_ctx());
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            media_token_result::Token(t) => {
                let t = t?;
                Ok(MediaToken {
                    token: text(t.get_token())?,
                    expires_at_ms: t.get_expires_at_ms(),
                })
            }
            media_token_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Removes the signed-in user's avatar.
    pub async fn remove_avatar(&self, call: &CallInfo) -> Result<(), ClientError> {
        let mut req = self.session.remove_avatar_request();
        call.write(req.get().init_ctx());
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            done::Ok(()) => Ok(()),
            done::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// The signed-in user's stored value for `key`, `None` when never set.
    pub async fn pref(&self, call: &CallInfo, key: &str) -> Result<Option<Vec<u8>>, ClientError> {
        let mut req = self.prefs.get_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_key(key);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            pref_result::Value(v) => Ok(Some(v?.to_vec())),
            pref_result::NotSet(()) => Ok(None),
            pref_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Stores `value` under `key` for the signed-in user.
    pub async fn set_pref(
        &self,
        call: &CallInfo,
        key: &str,
        value: &[u8],
    ) -> Result<(), ClientError> {
        let mut req = self.prefs.set_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_key(key);
            p.set_value(value);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            done::Ok(()) => Ok(()),
            done::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Sends `events` as one batch. The server logs and counts each one; a
    /// refusal (too many, or too fast) comes back as the error.
    pub async fn submit_events(
        &self,
        call: &CallInfo,
        events: &[ClientEvent],
    ) -> Result<(), ClientError> {
        let mut req = self.telemetry.submit_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            let mut list = p.reborrow().init_events(count(events.len()));
            for (i, event) in events.iter().enumerate() {
                let mut out = list.reborrow().get(count(i));
                out.set_kind(event.kind);
                out.set_level(event.level);
                out.set_route(&event.route);
                let mut fields = out.init_fields(count(event.fields.len()));
                for (j, (key, value)) in event.fields.iter().enumerate() {
                    let mut f = fields.reborrow().get(count(j));
                    f.set_key(key);
                    f.set_value(value);
                }
            }
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            done::Ok(()) => Ok(()),
            done::Error(e) => Err(ClientError::from_api(e?)),
        }
    }
}

impl PharosApi for NativeApi {
    type Playing = NativeGrant;

    async fn home(&self, call: &CallInfo) -> Result<Home, ClientError> {
        let mut req = self.library.home_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_latest_per_view(HOME_RAIL_LEN);
            p.set_resume_limit(HOME_RAIL_LEN);
            p.set_next_up_limit(HOME_RAIL_LEN);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            home_result::Home(home) => {
                let home = home?;
                let rails = home
                    .get_rails()?
                    .iter()
                    .map(|rail| {
                        Ok(HomeRail {
                            library: read_item(rail.get_view()?)?,
                            latest: read_items(rail.get_latest()?)?,
                        })
                    })
                    .collect::<Result<_, capnp::Error>>()?;
                Ok(Home {
                    rails,
                    resume: read_items(home.get_resume()?)?,
                    listening: read_items(home.get_listening()?)?,
                    next_up: read_items(home.get_next_up()?)?,
                })
            }
            home_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    async fn query(
        &self,
        call: &CallInfo,
        listing: &Listing,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError> {
        let mut req = self.library.query_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            let mut q = p.init_q();
            write_listing(q.reborrow(), listing);
            q.set_start(start);
            q.set_limit(limit);
        }
        let reply = req.send().promise.await?;
        read_page(reply.get()?.get_result()?)
    }

    async fn index(&self, call: &CallInfo, listing: &Listing) -> Result<ListIndex, ClientError> {
        let mut req = self.library.index_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            write_listing(p.init_q(), listing);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            index_result::Index(index) => {
                let index = index?;
                let letters = index
                    .get_letters()?
                    .iter()
                    .map(|l| Ok((text(l.get_letter())?, l.get_start())))
                    .filter(|l| !matches!(l, Ok((label, _)) if label.is_empty()))
                    .collect::<Result<_, capnp::Error>>()?;
                let genres = index
                    .get_genres()?
                    .iter()
                    .map(text)
                    .collect::<Result<_, capnp::Error>>()?;
                Ok(ListIndex { letters, genres })
            }
            index_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    async fn views(&self, call: &CallInfo) -> Result<Page, ClientError> {
        let mut req = self.library.views_request();
        call.write(req.get().init_ctx());
        let reply = req.send().promise.await?;
        read_page(reply.get()?.get_result()?)
    }

    async fn search(&self, call: &CallInfo, text: &str, limit: u32) -> Result<Page, ClientError> {
        let mut req = self.library.search_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_text(text);
            p.set_limit(limit);
        }
        let reply = req.send().promise.await?;
        read_page(reply.get()?.get_result()?)
    }

    async fn detail(&self, call: &CallInfo, id: ItemRef) -> Result<Detail, ClientError> {
        let mut req = self.library.detail_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            set_item_ref(p.init_id(), id);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            detail_result::Detail(d) => {
                let d = d?;
                Ok(Detail {
                    item: read_item(d.get_summary()?)?,
                    overview: text(d.get_overview())?,
                    genres: d
                        .get_genres()?
                        .iter()
                        .map(text)
                        .collect::<Result<_, capnp::Error>>()?,
                    chapters: d
                        .get_chapters()?
                        .iter()
                        .map(|c| {
                            Ok(Chapter {
                                name: text(c.get_name())?,
                                start_ms: c.get_start_ms(),
                            })
                        })
                        .collect::<Result<_, capnp::Error>>()?,
                    audio_tracks: read_tracks(d.get_audio_tracks()?)?,
                    subtitle_tracks: read_tracks(d.get_subtitle_tracks()?)?,
                    trickplay: d
                        .get_trickplay()?
                        .iter()
                        .map(|t| {
                            Ok(Trickplay {
                                width: t.get_width(),
                                height: t.get_height(),
                                interval_ms: t.get_interval_ms(),
                                thumb_count: t.get_thumb_count(),
                                tile_count: t.get_tile_count(),
                                grid: t.get_grid(),
                                tile_url: text(t.get_tile_url())?,
                            })
                        })
                        .collect::<Result<_, capnp::Error>>()?,
                    segments: d
                        .get_segments()?
                        .iter()
                        .filter_map(|s| {
                            // A segment kind newer than this build has no skip
                            // button to offer, so it is left out.
                            let kind = match s.get_kind().ok()? {
                                WireSegmentKind::Intro => SegmentKind::Intro,
                                WireSegmentKind::Outro => SegmentKind::Outro,
                                WireSegmentKind::Recap => SegmentKind::Recap,
                                WireSegmentKind::Preview => SegmentKind::Preview,
                                WireSegmentKind::Commercial => SegmentKind::Commercial,
                            };
                            Some(Segment {
                                kind,
                                start_ms: s.get_start_ms(),
                                end_ms: s.get_end_ms(),
                            })
                        })
                        .collect(),
                    fonts: d
                        .get_fonts()?
                        .iter()
                        .map(|f| {
                            Ok(Font {
                                index: f.get_index(),
                                filename: text(f.get_filename())?,
                                mime: text(f.get_mime())?,
                                url: text(f.get_url())?,
                            })
                        })
                        .collect::<Result<_, capnp::Error>>()?,
                    artists: d
                        .get_artists()?
                        .iter()
                        .map(text)
                        .collect::<Result<_, capnp::Error>>()?,
                    album: Some(text(d.get_album())?).filter(|a| !a.is_empty()),
                })
            }
            detail_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    async fn lyrics(
        &self,
        call: &CallInfo,
        id: ItemRef,
        want_words: bool,
    ) -> Result<Lyrics, ClientError> {
        let mut req = self.library.lyrics_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            set_item_ref(p.reborrow().init_id(), id);
            p.set_want_words(want_words);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            lyrics_result::Lyrics(l) => {
                let l = l?;
                Ok(Lyrics {
                    synced: l.get_synced(),
                    lines: l
                        .get_lines()?
                        .iter()
                        .map(|line| {
                            Ok(LyricLine {
                                start_ms: line.get_start_ms(),
                                text: text(line.get_text())?,
                                words: line
                                    .get_words()?
                                    .iter()
                                    .map(|w| LyricWord {
                                        at: w.get_at(),
                                        start_ms: w.get_start_ms(),
                                    })
                                    .collect(),
                            })
                        })
                        .collect::<Result<_, capnp::Error>>()?,
                    words_pending: l.get_words_pending(),
                })
            }
            lyrics_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    async fn mix(
        &self,
        call: &CallInfo,
        spec: &crate::mix::MixSpec,
        recent: &[ItemRef],
        count: u32,
    ) -> Result<crate::mix::MixBatch, ClientError> {
        use pharos_api::pharos_capnp::mix_result;
        let mut req = self.library.mix_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            crate::mix::write_spec(p.reborrow().init_spec(), spec);
            let mut ids = p
                .reborrow()
                .init_recent(u32::try_from(recent.len()).unwrap_or(u32::MAX));
            for (i, id) in (0..ids.len()).zip(recent) {
                set_item_ref(ids.reborrow().get(i), *id);
            }
            p.set_count(count);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            mix_result::Batch(b) => Ok(crate::mix::read_batch(b?)?),
            mix_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    async fn play(
        &self,
        call: &CallInfo,
        id: ItemRef,
        caps: &Capabilities,
        picks: TrackPicks,
    ) -> Result<(Decision, NativeGrant), ClientError> {
        let mut req = self.playback.negotiate_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            set_item_ref(p.reborrow().init_id(), id);
            let mut c = p.reborrow().init_caps();
            set_texts(
                c.reborrow().init_video_codecs(len(&caps.video_codecs)),
                &caps.video_codecs,
            );
            set_texts(
                c.reborrow().init_audio_codecs(len(&caps.audio_codecs)),
                &caps.audio_codecs,
            );
            set_texts(
                c.reborrow().init_containers(len(&caps.containers)),
                &caps.containers,
            );
            c.set_max_bitrate(caps.max_bitrate);
            c.set_hdr(caps.hdr);
            set_texts(
                c.reborrow()
                    .init_image_subtitles(len(&caps.image_subtitles)),
                &caps.image_subtitles,
            );
            set_choice(p.reborrow().init_audio(), picks.audio);
            set_choice(p.init_subtitle(), picks.subtitle);
        }
        let reply = req.send().promise.await?;
        let grant = match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            negotiate_result::Grant(grant) => grant?,
            negotiate_result::Error(e) => return Err(ClientError::from_api(e?)),
        };
        let mut req = grant.decision_request();
        call.write(req.get().init_ctx());
        let reply = req.send().promise.await?;
        let decision = match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            decision_result::Decision(d) => {
                let d = d?;
                Decision {
                    method: text(d.get_method())?,
                    direct_play_block: text(d.get_direct_play_block())?,
                    downgrade: text(d.get_downgrade())?,
                    stream_url: text(d.get_stream_url())?,
                    play_session: text(d.get_play_session()?.get_value())?,
                    audio: read_opt_u32(d.get_audio()?)?,
                    subtitle: read_opt_u32(d.get_subtitle()?)?,
                }
            }
            decision_result::Error(e) => return Err(ClientError::from_api(e?)),
        };
        Ok((decision, NativeGrant(grant)))
    }
}

/// Items per home rail: a screen's width of posters and some to scroll.
const HOME_RAIL_LEN: u32 = 24;

/// A [`PlaybackGrant`] over the native `PlaybackGrant` capability.
#[derive(Clone)]
pub struct NativeGrant(playback_grant::Client);

impl PlaybackGrant for NativeGrant {
    async fn report(
        &self,
        call: &CallInfo,
        position_ms: u64,
        paused: bool,
        playing: TrackPicks,
    ) -> Result<(), ClientError> {
        let mut req = self.0.report_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_position_ms(position_ms);
            p.set_paused(paused);
            set_choice(p.reborrow().init_audio(), playing.audio);
            set_choice(p.init_subtitle(), playing.subtitle);
        }
        let reply = req.send().promise.await?;
        crate::read_done(reply.get()?.get_result()?)
    }

    async fn stop(&self, call: &CallInfo, position_ms: u64) -> Result<(), ClientError> {
        let mut req = self.0.stop_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_position_ms(position_ms);
        }
        let reply = req.send().promise.await?;
        crate::read_done(reply.get()?.get_result()?)
    }
}

fn set_choice(mut out: stream_choice::Builder<'_>, choice: TrackChoice) {
    match choice {
        TrackChoice::Default => out.set_default(()),
        TrackChoice::Off => out.set_none(()),
        TrackChoice::Index(i) => out.set_index(i),
    }
}

fn read_tracks(
    list: capnp::struct_list::Reader<'_, track::Owned>,
) -> Result<Vec<Track>, capnp::Error> {
    list.iter()
        .map(|t| {
            Ok(Track {
                index: t.get_index(),
                language: text(t.get_language())?,
                title: text(t.get_title())?,
                codec: text(t.get_codec())?,
                is_default: t.get_is_default(),
                is_forced: t.get_is_forced(),
                is_hearing_impaired: t.get_is_hearing_impaired(),
                vtt_url: match read_delivery(t.get_delivery()) {
                    SubtitleDelivery::External => Some(text(t.get_url())?),
                    SubtitleDelivery::Burn | SubtitleDelivery::Image => None,
                },
                images_url: match read_delivery(t.get_delivery()) {
                    SubtitleDelivery::Image => Some(text(t.get_url())?),
                    SubtitleDelivery::External | SubtitleDelivery::Burn => None,
                },
                styled_url: Some(text(t.get_styled_url())?).filter(|u| !u.is_empty()),
            })
        })
        .collect()
}

pub(crate) fn text(t: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, capnp::Error> {
    Ok(t?.to_string()?)
}

fn len(list: &[String]) -> u32 {
    count(list.len())
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn set_texts(mut out: capnp::text_list::Builder<'_>, values: &[String]) {
    for (i, v) in (0u32..).zip(values) {
        out.set(i, v.as_str());
    }
}

pub(crate) fn set_item_ref(mut out: item_id::Builder<'_>, id: ItemRef) {
    match id {
        ItemRef::Real(n) => out.set_real(n),
        ItemRef::Synth(n) => out.set_synth(n),
    }
}

pub(crate) fn read_item_ref(id: item_id::Reader<'_>) -> Result<ItemRef, capnp::Error> {
    Ok(match id.which()? {
        item_id::Real(n) => ItemRef::Real(n),
        item_id::Synth(n) => ItemRef::Synth(n),
    })
}

/// The wire kind for `kind`; `None` for a kind this client does not know.
fn wire_kind(kind: ItemKind) -> Option<Kind> {
    Some(match kind {
        ItemKind::Movie => Kind::Movie,
        ItemKind::Episode => Kind::Episode,
        ItemKind::Audio => Kind::Audio,
        ItemKind::Book => Kind::Book,
        ItemKind::Series => Kind::Series,
        ItemKind::Season => Kind::Season,
        ItemKind::Album => Kind::Album,
        ItemKind::Artist => Kind::Artist,
        ItemKind::Person => Kind::Person,
        ItemKind::Collection => Kind::Collection,
        ItemKind::Playlist => Kind::Playlist,
        ItemKind::Channel => Kind::Channel,
        ItemKind::Library => Kind::Library,
        ItemKind::Other => return None,
    })
}

/// A delivery newer than this build is one it cannot draw, so it reads as
/// `burn`, the server-drawn fallback, rather than failing the whole detail.
fn read_delivery(delivery: Result<SubtitleDelivery, capnp::NotInSchema>) -> SubtitleDelivery {
    delivery.unwrap_or(SubtitleDelivery::Burn)
}

fn read_library_kind(kind: Result<WireLibraryKind, capnp::NotInSchema>) -> LibraryKind {
    match kind {
        Ok(WireLibraryKind::Movies) => LibraryKind::Movies,
        Ok(WireLibraryKind::TvShows) => LibraryKind::TvShows,
        Ok(WireLibraryKind::Music) => LibraryKind::Music,
        Ok(WireLibraryKind::Books) => LibraryKind::Books,
        Ok(WireLibraryKind::Mixed) | Err(_) => LibraryKind::Mixed,
    }
}

fn read_kind(kind: Result<Kind, capnp::NotInSchema>) -> ItemKind {
    let Ok(kind) = kind else {
        return ItemKind::Other;
    };
    match kind {
        Kind::Movie => ItemKind::Movie,
        Kind::Episode => ItemKind::Episode,
        Kind::Audio => ItemKind::Audio,
        Kind::Book => ItemKind::Book,
        Kind::Series => ItemKind::Series,
        Kind::Season => ItemKind::Season,
        Kind::Album => ItemKind::Album,
        Kind::Artist => ItemKind::Artist,
        Kind::Person => ItemKind::Person,
        Kind::Collection => ItemKind::Collection,
        Kind::Playlist => ItemKind::Playlist,
        Kind::Channel => ItemKind::Channel,
        Kind::Library => ItemKind::Library,
    }
}

fn read_opt_u16(o: opt_u16::Reader<'_>) -> Result<Option<u16>, capnp::Error> {
    Ok(match o.which()? {
        opt_u16::Some(n) => Some(n),
        opt_u16::None(()) => None,
    })
}

fn read_opt_u32(o: opt_u32::Reader<'_>) -> Result<Option<u32>, capnp::Error> {
    Ok(match o.which()? {
        opt_u32::Some(n) => Some(n),
        opt_u32::None(()) => None,
    })
}

/// An `ItemsResult` as a [`Page`], or the API error it carries.
/// `listing` into a wire `ItemQuery`, leaving its page unset.
fn write_listing(mut q: item_query::Builder<'_>, listing: &Listing) {
    set_item_ref(q.reborrow().init_parent_id(), listing.parent);
    if let Some(kind) = listing.group.and_then(wire_kind) {
        q.reborrow().init_kinds(1).set(0, kind);
    }
    q.set_sort_by(&listing.sort_by);
    q.set_descending(listing.descending);
    q.set_genre(listing.genre.as_deref().unwrap_or_default());
    let played = listing
        .played
        .map(|p| if p { "IsPlayed" } else { "IsUnplayed" });
    let mut filters = q.init_filters(u32::from(played.is_some()));
    if let Some(played) = played {
        filters.set(0, played);
    }
}

fn read_page(result: items_result::Reader<'_>) -> Result<Page, ClientError> {
    match result.which().map_err(capnp::Error::from)? {
        items_result::Page(page) => {
            let page = page?;
            Ok(Page {
                items: read_items(page.get_items()?)?,
                total: page.get_total(),
                start: page.get_start(),
            })
        }
        items_result::Error(e) => Err(ClientError::from_api(e?)),
    }
}

fn read_items(
    list: capnp::struct_list::Reader<'_, item_summary::Owned>,
) -> Result<Vec<Item>, capnp::Error> {
    list.iter().map(read_item).collect()
}

pub(crate) fn read_item(item: item_summary::Reader<'_>) -> Result<Item, capnp::Error> {
    let year = read_opt_u16(item.get_year()?)?;
    let image = item
        .get_images()?
        .iter()
        .next()
        .map(|i| text(i.get_url()))
        .transpose()?
        .filter(|u| !u.is_empty());
    let series_name = Some(text(item.get_series_name())?).filter(|s| !s.is_empty());
    let state = item.get_user_state()?;
    let kind = read_kind(item.get_kind());
    let parent = match item.has_parent_id() {
        true => Some(read_item_ref(item.get_parent_id()?)?),
        false => None,
    };
    Ok(Item {
        id: read_item_ref(item.get_id()?)?,
        kind,
        name: text(item.get_name())?,
        year,
        runtime_ms: item.get_runtime_ms(),
        image,
        played: state.get_played(),
        favorite: state.get_favorite(),
        position_ms: state.get_position_ms(),
        series_name,
        season: read_opt_u32(item.get_parent_index_number()?)?,
        episode: read_opt_u32(item.get_index_number()?)?,
        parent,
        library_kind: (kind == ItemKind::Library)
            .then(|| read_library_kind(item.get_library_kind())),
        imdb_rating: read_opt_u16(item.get_imdb_rating()?)?,
        critic_rating: read_opt_u16(item.get_critic_rating()?)?,
        artist: Some(text(item.get_artist())?).filter(|a| !a.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn item_refs_round_trip_through_urls() {
        for id in [ItemRef::Real(0), ItemRef::Real(42), ItemRef::Synth(7)] {
            assert_eq!(id.to_string().parse::<ItemRef>().unwrap(), id);
        }
        assert_eq!(ItemRef::Synth(7).to_string(), "s7");
        for bad in ["", "s", "x1", "-1", "s-1", "1.5", "+42", "s+7", " 1"] {
            assert!(bad.parse::<ItemRef>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_a_playlist_url_is_hls() {
        let decision = |method: &str, url: &str| Decision {
            method: method.into(),
            direct_play_block: String::new(),
            downgrade: String::new(),
            stream_url: url.into(),
            play_session: String::new(),
            audio: None,
            subtitle: None,
        };
        assert!(decision("transcode", "/videos/1/master.m3u8?PlaySessionId=p").is_hls());
        assert!(!decision("direct_play", "/videos/1/stream?static=true").is_hls());
        assert!(!decision("", "/videos/1/stream?static=true&x=.m3u8").is_hls());
    }

    #[test]
    fn a_kind_newer_than_the_client_reads_as_other() {
        assert_eq!(read_kind(Err(capnp::NotInSchema(99))), ItemKind::Other);
        assert_eq!(read_kind(Ok(Kind::Movie)), ItemKind::Movie);
    }

    /// B559 / V469: a track whose delivery is newer than this build reads
    /// whole, with neither URL, instead of failing the detail it is in.
    /// Disarm check: with `read_tracks` using `t.get_delivery()?`, the read
    /// fails with `NotInSchema(9)`.
    #[test]
    fn a_track_with_an_unknown_delivery_reads_with_no_subtitle_url() {
        let mut message = capnp::message::Builder::new_default();
        {
            let root: capnp::any_pointer::Builder<'_> = message.init_root();
            let mut list = root.initn_as::<capnp::struct_list::Builder<'_, track::Owned>>(1);
            let mut t = list.reborrow().get(0);
            t.set_index(7);
            t.set_url("/subtitles/7");
            t.set_delivery(SubtitleDelivery::Image);
        }
        let mut bytes = capnp::serialize::write_message_to_words(&message);
        // Segment table (8 bytes), list pointer, composite tag word, then the
        // element's data section: `delivery` is the u16 at data offset 3.
        let at = 8 + 8 + 8 + 3 * 2;
        assert_eq!(bytes[at..at + 2], [SubtitleDelivery::Image as u8, 0]);
        bytes[at] = 9;
        let reader = capnp::serialize::read_message_from_flat_slice(
            &mut bytes.as_slice(),
            capnp::message::ReaderOptions::new(),
        )
        .unwrap();
        let root: capnp::any_pointer::Reader<'_> = reader.get_root().unwrap();
        let tracks = read_tracks(root.get_as().unwrap()).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].index, 7);
        assert_eq!(
            (
                tracks[0].vtt_url.as_deref(),
                tracks[0].images_url.as_deref()
            ),
            (None, None)
        );
    }

    #[test]
    fn a_styled_url_reads_as_some_and_an_empty_one_as_none() {
        let mut message = capnp::message::Builder::new_default();
        {
            let root: capnp::any_pointer::Builder<'_> = message.init_root();
            let mut list = root.initn_as::<capnp::struct_list::Builder<'_, track::Owned>>(2);
            let mut t = list.reborrow().get(0);
            t.set_delivery(SubtitleDelivery::External);
            t.set_url("/videos/1/subtitles/2/stream.vtt");
            t.set_styled_url("/videos/1/subtitles/2/stream.ass");
            let mut plain = list.reborrow().get(1);
            plain.set_delivery(SubtitleDelivery::External);
            plain.set_url("/videos/1/subtitles/2/stream.vtt");
        }
        let reader = message
            .get_root_as_reader::<capnp::any_pointer::Reader<'_>>()
            .unwrap();
        let tracks = read_tracks(reader.get_as().unwrap()).unwrap();
        assert_eq!(
            tracks[0].styled_url.as_deref(),
            Some("/videos/1/subtitles/2/stream.ass")
        );
        assert_eq!(
            tracks[0].vtt_url.as_deref(),
            Some("/videos/1/subtitles/2/stream.vtt")
        );
        assert_eq!(tracks[1].styled_url, None);
    }

    #[test]
    fn a_subtitle_delivery_newer_than_the_client_reads_as_burn() {
        assert_eq!(
            read_delivery(Err(capnp::NotInSchema(9))),
            SubtitleDelivery::Burn
        );
        assert_eq!(
            read_delivery(Ok(SubtitleDelivery::Image)),
            SubtitleDelivery::Image
        );
    }
}
