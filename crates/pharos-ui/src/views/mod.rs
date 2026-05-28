//! Top-level app shell + sub-views.
//! T25 phase 2 wires LoginForm → LibraryView → PlayerView via Dioxus
//! signals + use_resource. The fetch client lives in `crate::client::web`
//! (feature `web`); host builds skip it but the components compile.

pub mod admin;
pub mod app_state;
pub mod detail;
pub mod group;
pub mod library;
pub mod live_tv;
pub mod login;
pub mod player;
pub mod prefs;
pub mod quick_connect;
pub mod remote;
pub mod search;
pub mod server_picker;

pub use admin::{AdminAction, AdminTab, AdminView, AdminViewProps, CreateUserAttempt};
pub use app_state::{AppRoute, RootApp};
pub use detail::{DetailAction, ItemDetailView};
pub use group::{GroupAction, GroupMember, GroupSessionPanel, GroupSnapshot};
pub use library::{ItemTile, LibraryView};
pub use live_tv::{LiveTvAction, LiveTvStatus, LiveTvView};
pub use login::{LoginAttempt, LoginForm};
pub use player::{PlaybackEvent, PlayerProps, PlayerView, QualityOption};
pub use prefs::{PrefsAction, PrefsTab, PrefsView};
pub use quick_connect::{
    QuickConnectAuthorizeAction, QuickConnectAuthorizeView, QuickConnectGuestAction,
    QuickConnectGuestStatus, QuickConnectGuestView,
};
pub use remote::{RemoteAction, RemoteControlView};
pub use search::{SearchStatus, SearchView};
pub use server_picker::{SavedServer, ServerPickerAction, ServerPickerView};

use dioxus::prelude::*;

/// Top-level mount point. Renders `RootApp` so the WASM entrypoint
/// stays a one-liner.
#[component]
pub fn App() -> Element {
    rsx! { RootApp {} }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn app_component_exists_and_is_callable() {
        // Renderer-free smoke: just confirm the function type resolves.
        let _: fn() -> Element = App;
    }
}
