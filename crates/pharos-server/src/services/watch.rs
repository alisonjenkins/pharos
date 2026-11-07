//! What a player shows around an item's video: its audio and subtitle tracks
//! and its trickplay sprites (013 FR-003, FR-004). The native `Library.detail`
//! serves these; the Jellyfin wire builds the same facts into its DTOs from
//! the same helpers.

use pharos_core::{MediaItem, MediaKind, SubtitleStyling};
use pharos_jellyfin_api::dto::{
    build_layout, build_media_streams_with_subtitles, sidecar_base_index, wire_item_id,
    MediaStreamDto, SubtitleStreamCtx,
};

use crate::api::jellyfin::subtitles::{
    discover_sidecars, is_image_subtitle_codec, sidecar_language_from_name,
};
use crate::state::AppState;
use crate::styled_track::styled_index;

/// One audio or subtitle stream. `index` is the absolute stream index a
/// negotiation takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub index: u32,
    pub language: Option<String>,
    pub title: Option<String>,
    pub codec: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    pub is_hearing_impaired: bool,
    /// How the player draws this subtitle itself. `None` for a subtitle that
    /// only plays burned into the video, and for every audio track.
    pub drawn: Option<Drawn>,
    /// The ASS script a renderer fetches with `?tier=&effects=` (spec 028
    /// FR-017); set only on the generated styled track.
    pub styled: Option<String>,
}

/// A subtitle the player draws over the video rather than one burned in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drawn {
    /// A text subtitle's WebVTT.
    Vtt(String),
    /// An image subtitle's timed PNG manifest (spec 019).
    Images(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tracks {
    pub audio: Vec<Track>,
    pub subtitles: Vec<Track>,
}

/// The item's audio tracks and its embedded and sidecar subtitles, in the
/// order PlaybackInfo lists them. Text subtitles of every format, ASS
/// included, are offered as WebVTT, which the player draws unless it has an
/// ASS renderer (ADR-0057) and fetches the script from the `stream.ass` beside
/// it. A `.ass`/`.ssa` sidecar carries its real codec so that renderer finds
/// it (026 FR-012, V489); the Jellyfin wire still lists every sidecar as
/// `webvtt`.
/// Image subtitles are offered as PNGs when `draws_images` (the server can
/// extract them, [`extracts_image_subtitles`]), else they burn.
///
/// `styling` is the item's current styling row ([`crate::styled_track::current`]);
/// when present the generated "English (Pharos styled)" track follows the
/// sidecars. It is native-only: the Jellyfin wire never lists it.
pub async fn tracks(
    item: &MediaItem,
    draws_images: bool,
    styling: Option<&SubtitleStyling>,
) -> Tracks {
    if !is_video(item) {
        return Tracks::default();
    }
    let id = wire_item_id(item.id);
    let mut out = Tracks::default();
    let sidecar_base = sidecar_base_index(&item.probe);
    let sidecars = discover_sidecars(&item.path).await;
    for mut s in streams(item).await {
        if s.kind == "Subtitle" && s.index >= sidecar_base {
            let offset = usize::try_from(s.index - sidecar_base).unwrap_or(usize::MAX);
            if let Some(codec) = sidecars
                .get(offset)
                .and_then(|(path, _)| sidecar_script_codec(path))
            {
                s.codec = Some(codec.to_owned());
            }
        }
        match s.kind {
            "Audio" => out.audio.push(track(s, None)),
            "Subtitle" => {
                let drawn = if s.is_text_subtitle_stream {
                    Some(Drawn::Vtt(format!(
                        "/videos/{id}/subtitles/{}/stream.vtt",
                        s.index
                    )))
                } else if draws_images && s.codec.as_deref().is_some_and(is_image_subtitle_codec) {
                    Some(Drawn::Images(format!(
                        "/videos/{id}/subtitles/{}/images.json",
                        s.index
                    )))
                } else {
                    None
                };
                out.subtitles.push(track(s, drawn));
            }
            _ => {}
        }
    }
    if let Some(row) = styling {
        let sidecar_count = u32::try_from(sidecars.len()).unwrap_or(u32::MAX);
        let index = styled_index(&item.probe, sidecar_count);
        out.subtitles.push(Track {
            index,
            language: Some("eng".to_owned()),
            title: Some(STYLED_TITLE.to_owned()),
            codec: Some("ass".to_owned()),
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
            drawn: Some(Drawn::Vtt(format!(
                "/videos/{id}/subtitles/{}/stream.vtt",
                row.source_index
            ))),
            styled: Some(format!("/videos/{id}/subtitles/{index}/stream.ass")),
        });
    }
    out
}

/// The generated track's title.
const STYLED_TITLE: &str = "English (Pharos styled)";

/// `ass` or `ssa` for a sidecar script of that extension; `None` for any
/// other sidecar, which stays WebVTT.
fn sidecar_script_codec(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("ass") {
        Some("ass")
    } else if ext.eq_ignore_ascii_case("ssa") {
        Some("ssa")
    } else {
        None
    }
}

/// Whether this server extracts image subtitles for a player to draw.
pub fn extracts_image_subtitles(state: &AppState) -> bool {
    state
        .subtitles
        .as_ref()
        .is_some_and(pharos_cache::subtitle_cache::SubtitleCache::extracts_images)
}

pub(crate) fn is_video(item: &MediaItem) -> bool {
    matches!(item.kind, MediaKind::Movie | MediaKind::Episode)
}

/// The item's streams as PlaybackInfo lists them, sidecar subtitles included,
/// for the track pickers in `services::playback` to choose from.
pub async fn streams(item: &MediaItem) -> Vec<MediaStreamDto> {
    let sidecars = discover_sidecars(&item.path).await;
    let ctx = SubtitleStreamCtx {
        sidecar_count: u32::try_from(sidecars.len()).unwrap_or(u32::MAX),
        sidecar_langs: sidecars
            .iter()
            .map(|(p, _)| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(sidecar_language_from_name)
            })
            .collect(),
        ..SubtitleStreamCtx::new(item.id)
    };
    build_media_streams_with_subtitles(&item.probe, is_video(item), Some(&ctx))
}

fn track(s: MediaStreamDto, drawn: Option<Drawn>) -> Track {
    Track {
        index: s.index,
        language: s.language,
        title: s.title,
        codec: s.codec,
        is_default: s.is_default,
        is_forced: s.is_forced,
        is_hearing_impaired: s.is_hearing_impaired,
        drawn,
        styled: None,
    }
}

/// One trickplay sprite set the server renders for an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trickplay {
    pub width: u32,
    pub height: u32,
    pub interval_ms: u32,
    pub thumb_count: u32,
    pub tile_count: u32,
    /// Thumbnails per tile side.
    pub grid: u32,
    /// The tile route with `{tile}` for the 0-based tile number.
    pub tile_url: String,
}

/// The sprite sets `/videos/{id}/trickplay/{width}/{tile}.jpg` serves for
/// `item`: one per configured width that has a layout. Empty when trickplay
/// is off, for audio, and for an item the probe gave no size or duration.
pub fn trickplay(state: &AppState, item: &MediaItem) -> Vec<Trickplay> {
    if state.trickplay.is_none() || !is_video(item) {
        return Vec::new();
    }
    let id = wire_item_id(item.id);
    state
        .trickplay_widths
        .iter()
        .filter_map(|&w| build_layout(&item.probe, w, state.trickplay_interval_ms))
        .map(|l| Trickplay {
            width: l.width,
            height: l.height,
            interval_ms: l.interval_ms,
            thumb_count: l.thumb_count,
            tile_count: l.tile_count,
            grid: pharos_cache::trickplay_cache::TILE_GRID,
            tile_url: format!("/videos/{id}/trickplay/{}/{{tile}}.jpg", l.width),
        })
        .collect()
}

/// A font the file embeds, for an ASS renderer (026 FR-010).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Font {
    /// The attachment's stream index: the `{n}` of its route.
    pub index: u32,
    pub filename: String,
    pub mime: String,
    /// `/videos/{id}/subtitles/{subtitle}/fonts/{n}` with `{subtitle}` left
    /// for the client to fill with the subtitle track it fetches for.
    pub url: String,
}

/// The fonts `item` embeds, in file order. Empty for audio and for a file
/// with no font attachment.
pub fn fonts(item: &MediaItem) -> Vec<Font> {
    if !is_video(item) {
        return Vec::new();
    }
    let id = wire_item_id(item.id);
    item.probe
        .attachments
        .iter()
        .filter(|a| crate::services::font_subset::is_font_attachment(a))
        .map(|a| Font {
            index: a.stream_index,
            filename: a.filename.clone().unwrap_or_default(),
            mime: crate::services::font_subset::font_mime(a).to_owned(),
            url: format!(
                "/videos/{id}/subtitles/{{subtitle}}/fonts/{}",
                a.stream_index
            ),
        })
        .collect()
}
