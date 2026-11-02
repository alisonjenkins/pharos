//! The UI's view of the server (009 `PharosApi` port): library and playback
//! as pharos domain calls, with plain Rust types in and out. [`NativeApi`]
//! implements it over a signed-in capnp `Session`; nothing above this module
//! sees a capnp type.

use std::fmt;
use std::str::FromStr;

use pharos_api::pharos_capnp::{
    decision_result, detail_result, home_result, item_id, item_summary, items_result, library,
    negotiate_result, playback, playback_grant, session, stream_choice, Kind,
};

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
        match s.strip_prefix('s') {
            Some(n) => n.parse().map(ItemRef::Synth).map_err(|_| bad()),
            None => s.parse().map(ItemRef::Real).map_err(|_| bad()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl ItemKind {
    /// Whether `Playback.negotiate` can play it directly.
    pub fn playable(self) -> bool {
        matches!(self, ItemKind::Movie | ItemKind::Episode | ItemKind::Audio)
    }
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
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeRail {
    pub library: Item,
    pub latest: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Home {
    pub rails: Vec<HomeRail>,
    pub resume: Vec<Item>,
    pub next_up: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    pub items: Vec<Item>,
    pub total: u32,
    pub start: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    pub item: Item,
    pub overview: String,
    pub genres: Vec<String>,
    pub chapters: Vec<Chapter>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub name: String,
    pub start_ms: u64,
}

/// What the player can decode, for the server to choose a delivery.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Capabilities {
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub containers: Vec<String>,
    /// Bits per second; 0 = no limit.
    pub max_bitrate: u64,
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
}

impl Decision {
    /// Whether `stream_url` is an HLS playlist rather than a file.
    pub fn is_hls(&self) -> bool {
        self.method != "direct_play"
    }
}

/// The calls the UI makes. Async methods without `Send`: the browser runs
/// everything on one thread.
#[allow(async_fn_in_trait)]
pub trait PharosApi {
    type Playing: PlaybackGrant;

    async fn home(&self, call: &CallInfo) -> Result<Home, ClientError>;
    /// The children of `parent` (a library, series, season, album), sorted
    /// by name, `limit` from `start`.
    async fn browse(
        &self,
        call: &CallInfo,
        parent: ItemRef,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError>;
    async fn detail(&self, call: &CallInfo, id: ItemRef) -> Result<Detail, ClientError>;
    /// Negotiate playback of `id` with the default audio track and no
    /// subtitles.
    async fn play(
        &self,
        call: &CallInfo,
        id: ItemRef,
        caps: &Capabilities,
    ) -> Result<(Decision, Self::Playing), ClientError>;
}

/// One negotiated playback, for progress reports.
#[allow(async_fn_in_trait)]
pub trait PlaybackGrant {
    async fn report(
        &self,
        call: &CallInfo,
        position_ms: u64,
        paused: bool,
    ) -> Result<(), ClientError>;
    async fn stop(&self, call: &CallInfo, position_ms: u64) -> Result<(), ClientError>;
}

/// [`PharosApi`] over a signed-in native session.
#[derive(Clone)]
pub struct NativeApi {
    library: library::Client,
    playback: playback::Client,
}

impl NativeApi {
    /// The capabilities are pipelined: no round trip until the first call.
    pub fn new(session: &session::Client) -> Self {
        Self {
            library: session.library_request().send().pipeline.get_library(),
            playback: session.playback_request().send().pipeline.get_playback(),
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
                    next_up: read_items(home.get_next_up()?)?,
                })
            }
            home_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    async fn browse(
        &self,
        call: &CallInfo,
        parent: ItemRef,
        start: u32,
        limit: u32,
    ) -> Result<Page, ClientError> {
        let mut req = self.library.query_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            let mut q = p.init_q();
            set_item_ref(q.reborrow().init_parent_id(), parent);
            q.set_sort_by("SortName");
            q.set_start(start);
            q.set_limit(limit);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
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
                })
            }
            detail_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    async fn play(
        &self,
        call: &CallInfo,
        id: ItemRef,
        caps: &Capabilities,
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
            p.reborrow().init_audio().set_default(());
            let mut subtitle: stream_choice::Builder<'_> = p.init_subtitle();
            subtitle.set_none(());
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
    ) -> Result<(), ClientError> {
        let mut req = self.0.report_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_position_ms(position_ms);
            p.set_paused(paused);
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

fn text(t: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, capnp::Error> {
    Ok(t?.to_string()?)
}

fn len(list: &[String]) -> u32 {
    u32::try_from(list.len()).unwrap_or(u32::MAX)
}

fn set_texts(mut out: capnp::text_list::Builder<'_>, values: &[String]) {
    for (i, v) in (0u32..).zip(values) {
        out.set(i, v.as_str());
    }
}

fn set_item_ref(mut out: item_id::Builder<'_>, id: ItemRef) {
    match id {
        ItemRef::Real(n) => out.set_real(n),
        ItemRef::Synth(n) => out.set_synth(n),
    }
}

fn read_item_ref(id: item_id::Reader<'_>) -> Result<ItemRef, capnp::Error> {
    Ok(match id.which()? {
        item_id::Real(n) => ItemRef::Real(n),
        item_id::Synth(n) => ItemRef::Synth(n),
    })
}

fn read_kind(kind: Kind) -> ItemKind {
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

fn read_items(
    list: capnp::struct_list::Reader<'_, item_summary::Owned>,
) -> Result<Vec<Item>, capnp::Error> {
    list.iter().map(read_item).collect()
}

fn read_item(item: item_summary::Reader<'_>) -> Result<Item, capnp::Error> {
    let opt_u32 =
        |o: pharos_api::pharos_capnp::opt_u32::Reader<'_>| -> Result<Option<u32>, capnp::Error> {
            Ok(match o.which()? {
                pharos_api::pharos_capnp::opt_u32::Some(n) => Some(n),
                pharos_api::pharos_capnp::opt_u32::None(()) => None,
            })
        };
    let year = match item.get_year()?.which()? {
        pharos_api::pharos_capnp::opt_u16::Some(y) => Some(y),
        pharos_api::pharos_capnp::opt_u16::None(()) => None,
    };
    let image = item
        .get_images()?
        .iter()
        .next()
        .map(|i| text(i.get_url()))
        .transpose()?
        .filter(|u| !u.is_empty());
    let series_name = Some(text(item.get_series_name())?).filter(|s| !s.is_empty());
    let state = item.get_user_state()?;
    Ok(Item {
        id: read_item_ref(item.get_id()?)?,
        kind: read_kind(item.get_kind()?),
        name: text(item.get_name())?,
        year,
        runtime_ms: item.get_runtime_ms(),
        image,
        played: state.get_played(),
        favorite: state.get_favorite(),
        position_ms: state.get_position_ms(),
        series_name,
        season: opt_u32(item.get_parent_index_number()?)?,
        episode: opt_u32(item.get_index_number()?)?,
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
        for bad in ["", "s", "x1", "-1", "s-1", "1.5"] {
            assert!(bad.parse::<ItemRef>().is_err(), "{bad:?}");
        }
    }
}
