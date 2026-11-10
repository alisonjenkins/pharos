//! The native web UI (009): screens over `pharos-client`'s `PharosApi`,
//! served at `/`. The older REST panes stay reachable at `/manage`.

pub mod account;
pub mod account_settings;
pub mod activity;
// 025: socket, token file and QuickConnect loop for the Android TV app.
#[cfg(target_os = "android")]
pub mod android;
pub mod app;
pub mod appearance;
pub mod approve_device;
pub mod art;
pub mod backdrop;
pub mod bars;
pub mod browse;
pub mod check_match;
pub mod damage;
pub mod depth;
pub mod diagnostics;
pub mod favourites;
pub mod find;
pub mod group_app;
// 014: the network Together client, shared by the browser and the TV app.
#[cfg(any(
    all(feature = "web", target_arch = "wasm32"),
    all(feature = "mobile", target_os = "android")
))]
mod group_rpc;
pub mod group_session;
pub mod hls_trace;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod hls_web;
pub mod kept;
pub mod kept_view;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod kept_web;
pub mod keymap;
pub mod keys;
pub mod letters;
pub mod listen;
pub mod live;
pub mod lyrics;
pub mod machine;
pub mod manage;
pub mod mixes;
pub mod my_devices;
pub mod paging;
pub mod palette;
pub mod person;
#[cfg(not(any(
    all(feature = "web", target_arch = "wasm32"),
    all(feature = "mobile", target_os = "android")
)))]
#[path = "host.rs"]
mod platform;
// 025: the Android TV app's WebView, reached through `document::eval`.
#[cfg(all(feature = "mobile", target_os = "android"))]
#[path = "android_page.rs"]
mod platform;
#[cfg(all(feature = "mobile", target_os = "android"))]
pub use platform::use_server_proxy;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
#[path = "web.rs"]
mod platform;
pub mod playback;
pub mod playback_settings;
/// For the non-native player's `<video>` (V455).
pub use platform::lift_cues_as_shown;

/// Moves keyboard focus to the element `id` once the render that adds it has
/// landed: for a confirm or form a button opens, and back to that button when
/// it closes.
pub fn focus_after_render(id: String) {
    dioxus::prelude::spawn(async move {
        platform::sleep_ms(0).await;
        platform::focus_by_id(&id);
    });
}
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod pdf_web;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod sw_web;
/// Registers the shell's service worker (ADR-0110); the browser build calls
/// it once at start.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub use sw_web::register as register_service_worker;
pub mod preplay;
pub mod privacy;
pub mod quality;
pub mod queue;
pub mod rails;
pub mod reader;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod reader_web;
pub mod report;
pub mod resume;
pub mod revisit;
pub mod route;
pub mod scrub;
pub mod seek_wait;
pub mod seen;
pub mod settings;
pub mod show_tracks;
pub mod stage;
pub mod stereo;
pub mod style_picker;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod subs_web;
pub mod subtitles;
pub mod together;
pub mod together_view;
pub mod track_follow;
pub mod trail;
pub mod turn;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod turn_web;
pub mod wait;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod xr_web;
// 025: Android TV sign-in logic (backoff, QuickConnect steps); no I/O, builds everywhere.
pub mod tv_auth;
// 025: the Android TV app's page rules (history, proxy, bridge); no I/O.
pub mod tv_page;
pub mod versions;
pub mod views;
pub mod visualiser;
pub mod watch_view;
pub mod whats_new;
