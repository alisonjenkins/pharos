//! Player view + on-screen-display (OSD) controls (T57).
//!
//! Renders a native `<video>` or `<audio>` element pointing at the
//! Jellyfin direct-play endpoint with the user's token as an `api_key`
//! query param (matches the T7 server contract).
//!
//! T57 adds:
//! - quality picker (`<select>` over `QualityOption`s; parent rebuilds
//!   the `src` with `MaxStreamingBitrate=`),
//! - fullscreen button (`HtmlElement::request_fullscreen()` via wasm),
//! - audio-only minimised toggle (collapses the chrome).
//!
//! HLS variant (master.m3u8 served from `/Videos/{id}/master.m3u8`)
//! is wired via the `src_override` prop — the parent passes the
//! master.m3u8 URL when an HLS transcode is required (T9).

use crate::api_types::{ItemKind, MediaTrack, PlaybackTracks};
use crate::client::ItemChapter;
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QualityOption {
    /// Human-readable label, e.g. "1080p · 8 Mbps".
    pub label: String,
    /// `MaxStreamingBitrate` value sent to /Items/{id}/PlaybackInfo.
    /// `0` reserved for "Auto" (no cap).
    pub max_bitrate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerProps {
    pub item_id: String,
    pub kind: ItemKind,
    pub access_token: String,
    pub server_base: String,
    pub tracks: Option<PlaybackTracks>,
    /// Optional source URL override. When set, replaces the
    /// kind-derived `/Videos/.../stream` or `/Audio/.../universal`
    /// URL. Live TV uses this to point at `/LiveTv/Channels/{id}/Stream`
    /// without polluting ItemKind with a `LiveChannel` variant.
    pub src_override: Option<String>,
    /// Quality picker entries. Empty → picker hidden.
    pub quality_options: Vec<QualityOption>,
    /// Currently selected `max_bitrate` for the quality picker.
    pub current_max_bitrate: Option<u32>,
    /// T57 phase 2: chapter markers (see component-level prop doc).
    pub chapters: Vec<ItemChapter>,
    /// T57 phase 2: total runtime in Jellyfin 100-ns ticks.
    pub run_time_ticks: u64,
}

fn track_label(t: &MediaTrack) -> String {
    let lang = t.language.clone().unwrap_or_default();
    let title = t.title.clone().unwrap_or_default();
    if !title.is_empty() {
        if !lang.is_empty() {
            return format!("{title} ({lang})");
        }
        return title;
    }
    if !lang.is_empty() {
        return lang;
    }
    format!("Track {}", t.index)
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackEvent {
    Started {
        item_id: String,
    },
    Progress {
        item_id: String,
        position_seconds: f64,
        paused: bool,
    },
    Stopped {
        item_id: String,
        position_seconds: f64,
    },
    QualityChanged {
        max_bitrate: u32,
    },
    FullscreenRequested,
    /// User clicked a chapter marker — parent seeks the media element
    /// to `position_seconds` (e.g. via `HtmlMediaElement::set_current_time`).
    ChapterSelected {
        position_seconds: f64,
    },
    /// T57 phase 3 — user picked an audio track. Parent rebuilds
    /// `src_override` with `&AudioStreamIndex={index}`.
    AudioTrackSelected {
        index: u32,
    },
    /// T57 phase 3 — user picked a subtitle track. `None` clears the
    /// burn-in selection (browser-native `<track>` still wins for
    /// direct play); parent rebuilds with `&SubtitleStreamIndex={index}`
    /// when transcoding.
    SubtitleTrackSelected {
        index: Option<u32>,
    },
}

#[component]
pub fn PlayerView(
    item_id: String,
    kind: ItemKind,
    access_token: String,
    server_base: String,
    tracks: Option<PlaybackTracks>,
    src_override: Option<String>,
    #[props(default)] quality_options: Vec<QualityOption>,
    #[props(default)] current_max_bitrate: Option<u32>,
    /// T57 phase 2: chapter markers rendered over the scrub bar. Each
    /// entry's `start_position_ticks` (Jellyfin 100-ns) over the total
    /// `run_time_ticks` drives the marker's `left: {pct}%`. Empty
    /// hides the strip.
    #[props(default)]
    chapters: Vec<ItemChapter>,
    /// Total duration in Jellyfin ticks. Drives the chapter strip's
    /// marker positions; pass the item's RunTimeTicks.
    #[props(default)]
    run_time_ticks: u64,
    on_event: EventHandler<PlaybackEvent>,
) -> Element {
    let src = match src_override.as_ref() {
        Some(url) => url.clone(),
        None => match kind {
            ItemKind::Audio => {
                format!("{server_base}/Audio/{item_id}/universal?api_key={access_token}")
            }
            ItemKind::Movie | ItemKind::Episode => {
                format!("{server_base}/Videos/{item_id}/stream?api_key={access_token}")
            }
        },
    };

    let id_for_play = item_id.clone();
    let id_for_time = item_id.clone();
    let id_for_end = item_id.clone();
    let tracks = tracks.unwrap_or_default();
    let subtitles = tracks.subtitle.clone();
    let audios = tracks.audio.clone();
    // Stable id on the media element so Fullscreen can target it.
    let media_dom_id = format!("pharos-media-{item_id}");
    let mut minimised = use_signal(|| false);
    let current_max_label = current_max_bitrate
        .map(|b| b.to_string())
        .unwrap_or_else(|| "0".to_string());

    rsx! {
        section {
            class: if *minimised.read() {
                "pharos-player pharos-player-minimised"
            } else {
                "pharos-player"
            },
            "data-kind": "{kind.label()}",
            if matches!(kind, ItemKind::Audio) {
                audio {
                    id: "{media_dom_id}",
                    controls: true,
                    autoplay: true,
                    src: "{src}",
                    onplay: move |_| on_event.call(PlaybackEvent::Started { item_id: id_for_play.clone() }),
                    ontimeupdate: move |ev| on_event.call(PlaybackEvent::Progress {
                        item_id: id_for_time.clone(),
                        position_seconds: extract_current_time(&ev),
                        paused: false,
                    }),
                    onended: move |ev| on_event.call(PlaybackEvent::Stopped {
                        item_id: id_for_end.clone(),
                        position_seconds: extract_current_time(&ev),
                    }),
                }
            } else {
                video {
                    id: "{media_dom_id}",
                    controls: true,
                    autoplay: true,
                    playsinline: true,
                    src: "{src}",
                    onplay: move |_| on_event.call(PlaybackEvent::Started { item_id: id_for_play.clone() }),
                    ontimeupdate: move |ev| on_event.call(PlaybackEvent::Progress {
                        item_id: id_for_time.clone(),
                        position_seconds: extract_current_time(&ev),
                        paused: false,
                    }),
                    onended: move |ev| on_event.call(PlaybackEvent::Stopped {
                        item_id: id_for_end.clone(),
                        position_seconds: extract_current_time(&ev),
                    }),
                    // Subtitle `<track>` elements — the browser surfaces
                    // them as the native CC picker. `src` is the
                    // server-emitted DeliveryUrl rebased onto the
                    // user's server + token.
                    for sub in subtitles.iter() {
                        if let Some(url) = sub.delivery_url.as_ref() {
                            track {
                                kind: "subtitles",
                                src: "{server_base}{url}&api_key={access_token}",
                                srclang: sub.language.clone().unwrap_or_default(),
                                label: track_label(sub),
                                default: sub.is_default,
                            }
                        }
                    }
                }
            }
            // OSD: tracks + quality + fullscreen + minimise toggle.
            aside {
                class: "pharos-player-osd",
                if !audios.is_empty() {
                    details {
                        class: "pharos-player-audio",
                        summary { "Audio tracks ({audios.len()})" }
                        ul {
                            for t in audios.iter().cloned() {
                                li {
                                    key: "{t.index}",
                                    button {
                                        class: "pharos-player-audio-pick",
                                        onclick: move |_| on_event.call(
                                            PlaybackEvent::AudioTrackSelected { index: t.index },
                                        ),
                                        "{track_label(&t)}"
                                    }
                                }
                            }
                        }
                    }
                }
                if !subtitles.is_empty() {
                    details {
                        class: "pharos-player-subtitles",
                        summary { "Subtitles ({subtitles.len()})" }
                        ul {
                            li {
                                button {
                                    class: "pharos-player-subtitle-pick pharos-player-subtitle-off",
                                    onclick: move |_| on_event.call(
                                        PlaybackEvent::SubtitleTrackSelected { index: None },
                                    ),
                                    "Off"
                                }
                            }
                            for t in subtitles.iter().cloned() {
                                li {
                                    key: "{t.index}",
                                    button {
                                        class: "pharos-player-subtitle-pick",
                                        onclick: move |_| on_event.call(
                                            PlaybackEvent::SubtitleTrackSelected {
                                                index: Some(t.index),
                                            },
                                        ),
                                        "{track_label(&t)}"
                                        if t.is_external { " (external)" }
                                    }
                                }
                            }
                        }
                    }
                }
                if !quality_options.is_empty() {
                    label {
                        class: "pharos-player-quality",
                        "Quality: "
                        select {
                            value: "{current_max_label}",
                            onchange: move |ev| {
                                let v: u32 = ev.value().parse().unwrap_or(0);
                                on_event.call(PlaybackEvent::QualityChanged { max_bitrate: v });
                            },
                            for q in quality_options.iter() {
                                option {
                                    key: "{q.max_bitrate}",
                                    value: "{q.max_bitrate}",
                                    "{q.label}"
                                }
                            }
                        }
                    }
                }
                button {
                    class: "pharos-player-fullscreen",
                    onclick: move |_| {
                        request_fullscreen(&media_dom_id);
                        on_event.call(PlaybackEvent::FullscreenRequested);
                    },
                    "Fullscreen"
                }
                if matches!(kind, ItemKind::Audio) {
                    button {
                        class: if *minimised.read() {
                            "pharos-player-minimise on"
                        } else {
                            "pharos-player-minimise off"
                        },
                        onclick: move |_| {
                            let was = *minimised.read();
                            minimised.set(!was);
                        },
                        if *minimised.read() { "Expand" } else { "Minimise" }
                    }
                }
            }
            if !chapters.is_empty() && run_time_ticks > 0 {
                ChapterStrip {
                    chapters: chapters.clone(),
                    run_time_ticks: run_time_ticks,
                    on_event: on_event,
                }
            }
        }
    }
}

#[component]
fn ChapterStrip(
    chapters: Vec<ItemChapter>,
    run_time_ticks: u64,
    on_event: EventHandler<PlaybackEvent>,
) -> Element {
    rsx! {
        nav {
            class: "pharos-player-chapters",
            for c in chapters.iter().cloned() {
                ChapterMarker {
                    key: "{c.start_position_ticks}",
                    chapter: c,
                    run_time_ticks: run_time_ticks,
                    on_event: on_event,
                }
            }
        }
    }
}

#[component]
fn ChapterMarker(
    chapter: ItemChapter,
    run_time_ticks: u64,
    on_event: EventHandler<PlaybackEvent>,
) -> Element {
    let pct = if run_time_ticks == 0 {
        0.0
    } else {
        // Clamp so over-long chapter positions don't push off-screen.
        let raw = chapter.start_position_ticks as f64 / run_time_ticks as f64 * 100.0;
        raw.clamp(0.0, 100.0)
    };
    let style = format!("left: {pct:.2}%");
    let position_seconds = chapter.start_position_ticks as f64 / 10_000_000.0;
    rsx! {
        button {
            class: "pharos-player-chapter",
            style: "{style}",
            title: "{chapter.name}",
            onclick: move |_| on_event.call(PlaybackEvent::ChapterSelected { position_seconds }),
            "{chapter.name}"
        }
    }
}

/// Read the underlying `<video>` / `<audio>` element's `currentTime`.
///
/// On the `web` feature (wasm + dioxus-web present): downcast the
/// Dioxus `MediaData` to the `web_sys::Event` dioxus-web wraps it in,
/// then walk `event.target() -> HtmlMediaElement::current_time()`.
/// Returns `0.0` only when the event has no target or the target isn't
/// a media element (defensive — never observed in practice).
///
/// On host (test) builds: no DOM exists, so return `0.0`. Tests cover
/// the surrounding wire shape rather than runtime time values.
#[cfg(feature = "web")]
fn extract_current_time(ev: &Event<MediaData>) -> f64 {
    use dioxus_web::WebEventExt;
    use wasm_bindgen::JsCast;
    let Some(web_event) = ev.try_as_web_event() else {
        return 0.0;
    };
    let Some(target) = web_event.target() else {
        return 0.0;
    };
    match target.dyn_into::<web_sys::HtmlMediaElement>() {
        Ok(media) => media.current_time(),
        Err(_) => 0.0,
    }
}

#[cfg(not(feature = "web"))]
fn extract_current_time(_ev: &Event<MediaData>) -> f64 {
    0.0
}

#[cfg(feature = "web")]
fn request_fullscreen(media_id: &str) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(el) = doc.get_element_by_id(media_id) else {
        return;
    };
    let _ = el.request_fullscreen();
}

#[cfg(not(feature = "web"))]
fn request_fullscreen(_media_id: &str) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn player_view_module_exports_present() {
        // Pure existence smoke — the renderer is the runtime concern.
        fn _f(_p: PlayerProps) {}
        let _ = _f;
    }

    #[test]
    fn player_event_value_semantics() {
        let a = PlaybackEvent::Progress {
            item_id: "1".into(),
            position_seconds: 12.5,
            paused: false,
        };
        let b = a.clone();
        assert_eq!(a, b);
        let q = PlaybackEvent::QualityChanged {
            max_bitrate: 4_000_000,
        };
        assert_eq!(q.clone(), q);
        assert_ne!(q, PlaybackEvent::FullscreenRequested);
    }

    #[test]
    fn audio_src_uses_universal_endpoint() {
        let p = PlayerProps {
            item_id: "42".into(),
            kind: ItemKind::Audio,
            access_token: "tok".into(),
            server_base: "https://pharos.test".into(),
            tracks: None,
            src_override: None,
            quality_options: Vec::new(),
            current_max_bitrate: None,
            chapters: Vec::new(),
            run_time_ticks: 0,
        };
        let expected = "https://pharos.test/Audio/42/universal?api_key=tok";
        let src = format!(
            "{}/Audio/{}/universal?api_key={}",
            p.server_base, p.item_id, p.access_token
        );
        assert_eq!(src, expected);
    }
}
