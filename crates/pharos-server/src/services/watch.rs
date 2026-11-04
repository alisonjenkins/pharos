//! What a player shows around an item's video: its audio and subtitle tracks
//! and its trickplay sprites (013 FR-003, FR-004). The native `Library.detail`
//! serves these; the Jellyfin wire builds the same facts into its DTOs from
//! the same helpers.

use pharos_core::{MediaItem, MediaKind};
use pharos_jellyfin_api::dto::{
    build_layout, build_media_streams_with_subtitles, wire_item_id, MediaStreamDto,
    SubtitleStreamCtx,
};

use crate::api::jellyfin::subtitles::{discover_sidecars, sidecar_language_from_name};
use crate::state::AppState;

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
    /// An audio track's channel count, when the probe read one.
    pub channels: Option<u32>,
    /// A text subtitle's WebVTT. `None` for an image subtitle, which only
    /// plays burned into the video, and for every audio track.
    pub vtt_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tracks {
    pub audio: Vec<Track>,
    pub subtitles: Vec<Track>,
}

/// The item's audio tracks and its embedded and sidecar subtitles, in the
/// order PlaybackInfo lists them. Text subtitles of every format, ASS
/// included, are offered as WebVTT: the native player renders no ASS (013
/// FR-003 leaves its renderer open), and a plain-text cue beats a burn.
pub async fn tracks(item: &MediaItem) -> Tracks {
    if !is_video(item) {
        return Tracks::default();
    }
    let id = wire_item_id(item.id);
    let mut out = Tracks::default();
    for s in streams(item).await {
        match s.kind {
            "Audio" => out.audio.push(track(s, None)),
            "Subtitle" => {
                let vtt = s
                    .is_text_subtitle_stream
                    .then(|| format!("/videos/{id}/subtitles/{}/stream.vtt", s.index));
                out.subtitles.push(track(s, vtt));
            }
            _ => {}
        }
    }
    out
}

fn is_video(item: &MediaItem) -> bool {
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

fn track(s: MediaStreamDto, vtt_url: Option<String>) -> Track {
    Track {
        index: s.index,
        language: s.language,
        title: s.title,
        codec: s.codec,
        is_default: s.is_default,
        is_forced: s.is_forced,
        is_hearing_impaired: s.is_hearing_impaired,
        channels: s.channels,
        vtt_url,
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
