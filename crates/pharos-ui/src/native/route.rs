//! The native UI's routes (009 contracts/serving.md): one typed value per
//! URL, so a link cannot name a page that does not exist. The server mounts
//! the app under [`MOUNT`] (ADR-0038); [`Route::path`] puts the prefix on and
//! [`Route::parse`] takes it off.

use pharos_client::api::ItemRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Home,
    /// A library, series, season or album: its children.
    Browse(ItemRef),
    Title(ItemRef),
    Watch(ItemRef),
    /// A book, full screen, `/read/{id}`.
    Read(ItemRef),
    /// Someone in a cast: what they appear in, `/person/{id}`.
    Person(ItemRef),
    SignIn,
    /// The older REST panes (admin, preferences, search, live TV) until they
    /// move to the native API.
    Manage,
    /// The theme picker and editor, under `/settings`.
    Appearance,
    /// The signed-in user's own settings (password), under `/settings`.
    Account,
    /// Approving a Quick Connect code shown on another device, under `/settings`.
    ApproveDevice,
    /// This device's playback choices (quality, subtitles, crossfade), under `/settings`.
    Playback,
    /// What this account shares with the server (diagnostics), under `/settings`.
    Privacy,
    /// The books kept on this device for reading offline, under `/settings`.
    Offline,
    /// `/find`, where the search screen was: opens the palette over home.
    Find,
    /// The viewer's favourites, a rail per kind, `/favourites` (B1083).
    Favourites,
    /// Watch together (014): the groups to join, `/together`.
    Together,
    /// An invite link to one group, `/together/{id}`. The id is as the
    /// server minted it, lowercase (paths are lowercased on parse).
    TogetherGroup(String),
    /// No route matched; carries the path for the message.
    NotFound(String),
}

/// The product name, as the browser tab shows it.
pub const APP_NAME: &str = "Pharos";

/// Where the server mounts the app, and Dioxus.toml's `base_path`.
pub const MOUNT: &str = "/ui";

impl Route {
    /// The route for a URL path, with or without the [`MOUNT`] prefix.
    /// Case-insensitive, as the server lowercases paths before routing; a
    /// trailing slash is ignored, and so is a query or fragment: a library's
    /// filter rides in its URL (`?genre=…`, B560), and the TV app's history
    /// hands back the whole path with it (B1276).
    pub fn parse(path: &str) -> Route {
        let bare = path.split(['?', '#']).next().unwrap_or_default();
        let lower = bare.to_ascii_lowercase();
        let mut segments: Vec<&str> = lower.split('/').filter(|s| !s.is_empty()).collect();
        if segments.first() == Some(&MOUNT.trim_start_matches('/')) {
            segments.remove(0);
        }
        let id = |s: &str| s.parse::<ItemRef>().ok();
        match segments.as_slice() {
            [] => Route::Home,
            ["browse", s] => id(s).map_or_else(|| not_found(path), Route::Browse),
            ["title", s] => id(s).map_or_else(|| not_found(path), Route::Title),
            ["watch", s] => id(s).map_or_else(|| not_found(path), Route::Watch),
            ["read", s] => id(s).map_or_else(|| not_found(path), Route::Read),
            ["person", s] => id(s).map_or_else(|| not_found(path), Route::Person),
            ["signin"] => Route::SignIn,
            ["manage"] => Route::Manage,
            ["find"] => Route::Find,
            ["settings"] | ["settings", "appearance"] => Route::Appearance,
            ["favourites"] => Route::Favourites,
            ["together"] => Route::Together,
            ["together", g] if is_group_id(g) => Route::TogetherGroup((*g).to_owned()),
            ["settings", "account"] => Route::Account,
            ["settings", "approve-device"] => Route::ApproveDevice,
            ["settings", "playback"] => Route::Playback,
            ["settings", "privacy"] => Route::Privacy,
            ["settings", "offline"] => Route::Offline,
            _ => not_found(path),
        }
    }

    /// The URL path for this route; `parse(route.path()) == route`.
    pub fn path(&self) -> String {
        match self {
            Route::Home => format!("{MOUNT}/"),
            Route::Browse(id) => format!("{MOUNT}/browse/{id}"),
            Route::Title(id) => format!("{MOUNT}/title/{id}"),
            Route::Watch(id) => format!("{MOUNT}/watch/{id}"),
            Route::Read(id) => format!("{MOUNT}/read/{id}"),
            Route::Person(id) => format!("{MOUNT}/person/{id}"),
            Route::SignIn => format!("{MOUNT}/signin"),
            Route::Manage => format!("{MOUNT}/manage"),
            Route::Appearance => format!("{MOUNT}/settings/appearance"),
            Route::Account => format!("{MOUNT}/settings/account"),
            Route::ApproveDevice => format!("{MOUNT}/settings/approve-device"),
            Route::Playback => format!("{MOUNT}/settings/playback"),
            Route::Privacy => format!("{MOUNT}/settings/privacy"),
            Route::Offline => format!("{MOUNT}/settings/offline"),
            Route::Find => format!("{MOUNT}/find"),
            Route::Favourites => format!("{MOUNT}/favourites"),
            Route::Together => format!("{MOUNT}/together"),
            Route::TogetherGroup(id) => format!("{MOUNT}/together/{id}"),
            Route::NotFound(path) => path.clone(),
        }
    }

    /// The item the page shows, if it shows one.
    pub fn item(&self) -> Option<ItemRef> {
        match self {
            Route::Browse(id)
            | Route::Title(id)
            | Route::Watch(id)
            | Route::Read(id)
            | Route::Person(id) => Some(*id),
            _ => None,
        }
    }

    /// The browser tab's title: the page's name, or `name` (the item on the
    /// page, once loaded), then the app's.
    pub fn page_title(&self, name: Option<&str>) -> String {
        let page = match self {
            Route::Home | Route::SignIn => None,
            Route::Browse(_)
            | Route::Title(_)
            | Route::Watch(_)
            | Route::Read(_)
            | Route::Person(_) => name,
            Route::Manage => Some("Manage"),
            Route::Appearance => Some("Appearance"),
            Route::Account => Some("Account"),
            Route::ApproveDevice => Some("Approve a device"),
            Route::Playback => Some("Playback"),
            Route::Privacy => Some("Privacy"),
            Route::Offline => Some("Offline"),
            Route::Find => Some("Search"),
            Route::Favourites => Some("Favourites"),
            Route::Together | Route::TogetherGroup(_) => Some("Watch together"),
            Route::NotFound(_) => Some("Not found"),
        };
        match page {
            Some(page) => format!("{page} · {APP_NAME}"),
            None => APP_NAME.to_owned(),
        }
    }

    /// A bounded name for `CallContext.uiRoute`: never an id.
    pub fn name(&self) -> &'static str {
        match self {
            Route::Home => "home",
            Route::Browse(_) => "browse",
            Route::Title(_) => "title",
            Route::Watch(_) => "watch",
            Route::Read(_) => "read",
            Route::Person(_) => "person",
            Route::SignIn => "signin",
            Route::Manage => "manage",
            Route::Appearance => "appearance",
            Route::Account => "account",
            Route::ApproveDevice => "approve_device",
            Route::Playback => "playback",
            Route::Privacy => "privacy",
            Route::Offline => "offline",
            Route::Find => "find",
            Route::Favourites => "favourites",
            Route::Together => "together",
            Route::TogetherGroup(_) => "together_group",
            Route::NotFound(_) => "not_found",
        }
    }
}

/// A group id is a token of letters, digits, `-` and `_`.
fn is_group_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn not_found(path: &str) -> Route {
    Route::NotFound(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_round_trips_through_its_path() {
        for route in [
            Route::Home,
            Route::Browse(ItemRef::Real(4)),
            Route::Browse(ItemRef::Synth(9)),
            Route::Title(ItemRef::Real(42)),
            Route::Watch(ItemRef::Real(42)),
            Route::Read(ItemRef::Real(42)),
            Route::Person(ItemRef::Synth(77)),
            Route::SignIn,
            Route::Manage,
            Route::Appearance,
            Route::Account,
            Route::ApproveDevice,
            Route::Playback,
            Route::Privacy,
            Route::Offline,
            Route::Find,
            Route::Favourites,
            Route::Together,
            Route::TogetherGroup("0a1b-2c_3d".into()),
        ] {
            assert_eq!(Route::parse(&route.path()), route, "{route:?}");
        }
    }

    #[test]
    fn the_tab_names_the_page_then_the_app() {
        assert_eq!(Route::Home.page_title(None), "Pharos");
        assert_eq!(Route::Appearance.page_title(None), "Appearance · Pharos");
        let title = Route::Title(ItemRef::Real(42));
        assert_eq!(title.page_title(Some("Film AX")), "Film AX · Pharos");
        // Still loading: the app's name, never the last page's.
        assert_eq!(title.page_title(None), "Pharos");
        assert_eq!(Route::Home.page_title(Some("Film AX")), "Pharos");
    }

    #[test]
    fn the_reader_has_its_own_path_name_and_item() {
        let read = Route::Read(ItemRef::Real(9));
        assert_eq!(read.path(), "/ui/read/9");
        assert_eq!(Route::parse("/ui/read/9/"), read);
        assert_eq!(read.name(), "read");
        assert_eq!(read.item(), Some(ItemRef::Real(9)));
        assert_eq!(read.page_title(Some("Film AU")), "Film AU · Pharos");
        assert_eq!(
            Route::parse("/read/abc"),
            Route::NotFound("/read/abc".into())
        );
    }

    #[test]
    fn case_and_trailing_slashes_do_not_matter() {
        assert_eq!(Route::parse("/Title/S7/"), Route::Title(ItemRef::Synth(7)));
        assert_eq!(
            Route::parse("/ui/browse/s7?genre=animation"),
            Route::Browse(ItemRef::Synth(7)),
            "a library's filter query is not part of its route"
        );
        assert_eq!(
            Route::parse("/ui/title/s7#cast"),
            Route::Title(ItemRef::Synth(7))
        );
        assert_eq!(Route::parse(""), Route::Home);
    }

    #[test]
    fn unknown_paths_and_bad_ids_are_not_found() {
        for path in ["/title", "/title/abc", "/title/1/2", "/nope", "/watch/+1"] {
            assert_eq!(Route::parse(path), Route::NotFound(path.into()), "{path}");
        }
    }

    #[test]
    fn paths_carry_the_mount_prefix() {
        assert_eq!(Route::Home.path(), "/ui/");
        assert_eq!(Route::Title(ItemRef::Real(3)).path(), "/ui/title/3");
        assert_eq!(Route::Appearance.path(), "/ui/settings/appearance");
        assert_eq!(Route::Playback.path(), "/ui/settings/playback");
        assert_eq!(Route::Privacy.path(), "/ui/settings/privacy");
        assert_eq!(Route::Offline.path(), "/ui/settings/offline");
    }

    #[test]
    fn bare_settings_opens_appearance() {
        assert_eq!(Route::parse("/ui/settings"), Route::Appearance);
        assert_eq!(Route::parse("/ui/settings/"), Route::Appearance);
    }

    #[test]
    fn together_routes_parse_and_format() {
        assert_eq!(Route::parse("/ui/together"), Route::Together);
        assert_eq!(Route::parse("/together/"), Route::Together);
        assert_eq!(
            Route::parse("/ui/together/AB12"),
            Route::TogetherGroup("ab12".into())
        );
        assert_eq!(Route::Together.path(), "/ui/together");
        assert_eq!(
            Route::TogetherGroup("ab12".into()).path(),
            "/ui/together/ab12"
        );
        assert_eq!(Route::Together.name(), "together");
        assert_eq!(Route::TogetherGroup("x".into()).name(), "together_group");
        for path in ["/together/a/b", "/together/a%20b", "/together/a.b"] {
            assert_eq!(Route::parse(path), Route::NotFound(path.into()), "{path}");
        }
    }

    #[test]
    fn the_prefix_is_optional_and_case_insensitive_on_parse() {
        assert_eq!(Route::parse("/ui"), Route::Home);
        assert_eq!(Route::parse("/UI/"), Route::Home);
        assert_eq!(Route::parse("/Ui/Find"), Route::Find);
        assert_eq!(Route::parse("/find"), Route::Find);
        assert_eq!(Route::parse("/ui/ui"), Route::NotFound("/ui/ui".into()));
    }
}
