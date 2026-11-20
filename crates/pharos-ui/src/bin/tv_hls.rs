//! The Rust HLS engine, for the Android app's page (spec 031).
//!
//! The app's Rust runs natively and its page in the system WebView, where
//! `MediaSource` lives. This crate's engine (`native::hls_page`), built to
//! wasm with `--features web`, runs there instead: `scripts/tv-hls.sh` builds
//! it and the APK carries it (`assets/generated/`).

use pharos_ui::native::hls_page;
use wasm_bindgen::prelude::*;

/// A running play; freeing it ends the play.
#[wasm_bindgen]
pub struct PagePlay {
    _play: hls_page::PagePlay,
}

/// Play `url` on `<video id=video_id>` from `start_ms`. `send` gets each
/// event as JSON (`tv_page::EngineEvent`).
#[wasm_bindgen]
pub fn start(
    video_id: String,
    url: String,
    start_ms: f64,
    device: String,
    token: Option<String>,
    send: js_sys::Function,
) -> Result<PagePlay, JsValue> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let start_ms = start_ms.max(0.0) as u64;
    hls_page::start(
        hls_page::Start {
            video_id: &video_id,
            url: &url,
            start_ms,
            device: &device,
            token,
        },
        move |json| {
            if let Err(e) = send.call1(&JsValue::NULL, &JsValue::from_str(&json)) {
                web_sys::console::warn_1(&e);
            }
        },
    )
    .map(|play| PagePlay { _play: play })
    .map_err(|e| JsValue::from_str(&e))
}

fn main() {}
