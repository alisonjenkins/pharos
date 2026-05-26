//! Dioxus components for pharos. Renderer-agnostic library; the WASM
//! entrypoint + trunk pipeline land in T24 phase 2.
//!
//! V16: components consume only the public Jellyfin-compat API. Anything
//! that needs server-private state goes through the public API, not a
//! backdoor.

#![allow(non_snake_case)]

pub mod views;

pub use views::App;
