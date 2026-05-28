//! Dioxus components for pharos. Renderer-agnostic library; the WASM
//! entrypoint + trunk pipeline land in T24 phase 2.
//!
//! V16: components consume only the public Jellyfin-compat API. Anything
//! that needs server-private state goes through the public API, not a
//! backdoor.

#![allow(non_snake_case)]

pub mod api_types;
pub mod client;
pub mod views;

pub use api_types::{ItemKind, LibraryItem, LoggedInUser};
pub use client::{
    parse_auth_response, parse_items_response, ActivityEntry, AdminUser, ApiKey, ClientError,
    DeviceEntry, ItemChapter, ItemDetail, ItemPerson, LibraryFolder, LiveChannel, LiveProgram,
    LogEntry, NewApiKey, PluginEntry, RemoteSession, ScheduledTask, SearchHint, UserConfiguration,
};
pub use views::{
    AdminAction, AdminTab, AdminView, App, AppRoute, DetailAction, GroupAction, GroupMember,
    GroupSessionPanel, GroupSnapshot, ItemDetailView, ItemTile, LibraryView, LiveTvAction,
    LiveTvStatus, LiveTvView, LoginAttempt, LoginForm, PlaybackEvent, PlayerProps, PlayerView,
    PrefsAction, PrefsTab, PrefsView, QualityOption, RemoteAction, RemoteControlView, SavedServer,
    SearchStatus, SearchView, ServerPickerAction, ServerPickerView,
};
