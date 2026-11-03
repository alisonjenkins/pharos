//! The native web UI (009): screens over `pharos-client`'s `PharosApi`,
//! served at `/`. The older REST panes stay reachable at `/manage`.

pub mod app;
pub mod appearance;
pub mod backdrop;
pub mod browse;
pub mod depth;
pub mod diagnostics;
pub mod find;
pub mod letters;
pub mod listen;
pub mod machine;
pub mod paging;
#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
#[path = "host.rs"]
mod platform;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
#[path = "web.rs"]
mod platform;
pub mod playback;
pub mod route;
pub mod together;
pub mod together_view;
pub mod views;
pub mod visualiser;
pub mod watch_view;
