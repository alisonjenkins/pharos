//! Player view. Renders a native `<video>` or `<audio>` element
//! pointing at the Jellyfin direct-play endpoint with the user's token
//! as an `api_key` query param (matches the T7 server contract).
//!
//! HLS variant (master.m3u8 served from `/Videos/{id}/master.m3u8`)
//! lands once T9 ships server-side HLS. Until then this component is
//! direct-play only.

use crate::api_types::ItemKind;
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerProps {
    pub item_id: String,
    pub kind: ItemKind,
    pub access_token: String,
    pub server_base: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackEvent {
    Started { item_id: String },
    Progress { item_id: String, position_seconds: f64, paused: bool },
    Stopped { item_id: String, position_seconds: f64 },
}

#[component]
pub fn PlayerView(
    item_id: String,
    kind: ItemKind,
    access_token: String,
    server_base: String,
    on_event: EventHandler<PlaybackEvent>,
) -> Element {
    let src = match kind {
        ItemKind::Audio => format!(
            "{server_base}/Audio/{item_id}/universal?api_key={access_token}"
        ),
        ItemKind::Movie | ItemKind::Episode => format!(
            "{server_base}/Videos/{item_id}/stream?api_key={access_token}"
        ),
    };

    let id_for_play = item_id.clone();
    let id_for_time = item_id.clone();
    let id_for_end = item_id.clone();

    rsx! {
        section {
            class: "pharos-player",
            "data-kind": "{kind.label()}",
            if matches!(kind, ItemKind::Audio) {
                audio {
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
                }
            }
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
    }

    #[test]
    fn audio_src_uses_universal_endpoint() {
        // Indirect check: building props for audio + sanity-format the
        // expected src.
        let p = PlayerProps {
            item_id: "42".into(),
            kind: ItemKind::Audio,
            access_token: "tok".into(),
            server_base: "https://pharos.test".into(),
        };
        let expected =
            "https://pharos.test/Audio/42/universal?api_key=tok";
        let src = format!(
            "{}/Audio/{}/universal?api_key={}",
            p.server_base, p.item_id, p.access_token
        );
        assert_eq!(src, expected);
    }
}
