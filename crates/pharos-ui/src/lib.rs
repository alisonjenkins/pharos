//! Dioxus components for pharos. Renderer-agnostic library; the WASM
//! entrypoint + trunk pipeline land in T24 phase 2.
//!
//! V16: components consume only the public Jellyfin-compat API. Anything
//! that needs server-private state goes through the public API, not a
//! backdoor.

#![allow(non_snake_case)]

pub mod api_types;
pub mod client;
pub mod native;
pub mod render;
pub mod theme;
pub mod views;

pub use api_types::{ItemKind, LibraryItem, LoggedInUser};
pub use client::{
    parse_auth_response, parse_items_response, ActivityEntry, AdminUser, ApiKey, BrandingConfig,
    ClientError, DeviceEntry, ItemChapter, ItemDetail, ItemPerson, LibraryFolder, LiveChannel,
    LiveProgram, LocalizationCulture, LogEntry, NewApiKey, PluginEntry, QuickConnectInitiate,
    QuickConnectPoll, RemoteSession, ScheduledTask, SearchHint, UserConfiguration,
};
pub use views::{
    AdminAction, AdminTab, AdminView, App, AppRoute, DetailAction, GroupAction, GroupMember,
    GroupSessionPanel, GroupSnapshot, ItemDetailView, ItemTile, LibraryView, LiveTvAction,
    LiveTvStatus, LiveTvView, LoginAttempt, LoginForm, PlaybackEvent, PlayerProps, PlayerView,
    PrefsAction, PrefsTab, PrefsView, QualityOption, RemoteAction, RemoteControlView, SavedServer,
    SearchStatus, SearchView, ServerPickerAction, ServerPickerView,
};

/// The shipped bundle's build settings, which live outside this crate
/// (workspace `Cargo.toml`, `flake.nix`) and have no other harness.
#[cfg(test)]
mod bundle_build_tests {
    fn repo_file(rel: &str) -> String {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../");
        std::fs::read_to_string(format!("{root}{rel}")).unwrap_or_default()
    }

    /// V763 (B813): the web bundle builds with the size-optimised profile.
    #[test]
    fn the_web_bundle_builds_for_size() {
        let cargo = repo_file("Cargo.toml");
        let profile = cargo
            .split("[profile.wasm-release]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .unwrap_or_default();
        for setting in [
            "inherits = \"release\"",
            "opt-level = \"z\"",
            "lto = \"fat\"",
            "codegen-units = 1",
        ] {
            assert!(profile.contains(setting), "wasm-release lacks `{setting}`");
        }
        // B837: strip removes the section wasm-bindgen needs.
        assert!(
            !profile.contains("strip = true"),
            "wasm-release must not strip: wasm-bindgen needs target_features"
        );
        let flake = repo_file("flake.nix");
        assert!(
            flake.contains("--bin pharos-ui-web --features web --release --profile wasm-release"),
            "pharosUiBundle must build with --profile wasm-release"
        );
    }

    /// V764 (B814): the proxy compresses the wasm and the bundled font.
    #[test]
    fn the_proxy_compresses_the_wasm() {
        let flake = repo_file("flake.nix");
        let types = flake
            .split("gzip_types\n")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap_or_default();
        for mime in ["application/wasm", "font/ttf"] {
            assert!(types.contains(mime), "gzip_types lacks {mime}");
        }
        assert!(
            flake.contains("font/ttf ttf;"),
            "angie must type .ttf as font/ttf"
        );
    }
}
