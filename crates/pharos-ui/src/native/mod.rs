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
pub mod backdrop;
pub mod browse;
pub mod depth;
pub mod diagnostics;
pub mod find;
pub mod group_app;
pub mod group_session;
pub mod letters;
pub mod listen;
pub mod lyrics;
pub mod machine;
pub mod mixes;
pub mod paging;
pub mod palette;
#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
#[path = "host.rs"]
mod platform;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
#[path = "web.rs"]
mod platform;
pub mod playback;
/// For the non-native player's `<video>` (V455).
pub use platform::lift_cues_as_shown;
pub mod quality;
pub mod queue;
pub mod rails;
pub mod resume;
pub mod route;
pub mod seen;
pub mod stage;
pub mod style_picker;
pub mod together;
pub mod together_view;
pub mod track_follow;
pub mod trail;
// 025: Android TV sign-in logic (backoff, QuickConnect steps); no I/O, builds everywhere.
pub mod tv_auth;
pub mod views;
pub mod visualiser;
pub mod watch_view;
