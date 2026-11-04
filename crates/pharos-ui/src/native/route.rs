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
    /// The search screen, `/find`: matches from `Library.search` as you type.
    Find,
    /// Watch together (014): the groups to join, `/together`.
    Together,
    /// An invite link to one group, `/together/{id}`. The id is as the
    /// server minted it, lowercase (paths are lowercased on parse).
    TogetherGroup(String),
    /// No route matched; carries the path for the message.
    NotFound(String),
}

/// Where the server mounts the app, and Dioxus.toml's `base_path`.
pub const MOUNT: &str = "/ui";

impl Route {
    /// The route for a URL path, with or without the [`MOUNT`] prefix.
    /// Case-insensitive, as the server lowercases paths before routing; a
    /// trailing slash is ignored.
    pub fn parse(path: &str) -> Route {
        let lower = path.to_ascii_lowercase();
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
            ["signin"] => Route::SignIn,
            ["manage"] => Route::Manage,
            ["find"] => Route::Find,
            ["settings", "appearance"] => Route::Appearance,
            ["together"] => Route::Together,
            ["together", g] if is_group_id(g) => Route::TogetherGroup((*g).to_owned()),
            ["settings", "account"] => Route::Account,
            ["settings", "approve-device"] => Route::ApproveDevice,
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
            Route::SignIn => format!("{MOUNT}/signin"),
            Route::Manage => format!("{MOUNT}/manage"),
            Route::Appearance => format!("{MOUNT}/settings/appearance"),
            Route::Account => format!("{MOUNT}/settings/account"),
            Route::ApproveDevice => format!("{MOUNT}/settings/approve-device"),
            Route::Find => format!("{MOUNT}/find"),
            Route::Together => format!("{MOUNT}/together"),
            Route::TogetherGroup(id) => format!("{MOUNT}/together/{id}"),
            Route::NotFound(path) => path.clone(),
        }
    }

    /// A bounded name for `CallContext.uiRoute`: never an id.
    pub fn name(&self) -> &'static str {
        match self {
            Route::Home => "home",
            Route::Browse(_) => "browse",
            Route::Title(_) => "title",
            Route::Watch(_) => "watch",
            Route::SignIn => "signin",
            Route::Manage => "manage",
            Route::Appearance => "appearance",
            Route::Account => "account",
            Route::ApproveDevice => "approve_device",
            Route::Find => "find",
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
            Route::SignIn,
            Route::Manage,
            Route::Appearance,
            Route::Account,
            Route::ApproveDevice,
            Route::Find,
            Route::Together,
            Route::TogetherGroup("0a1b-2c_3d".into()),
        ] {
            assert_eq!(Route::parse(&route.path()), route, "{route:?}");
        }
    }

    #[test]
    fn case_and_trailing_slashes_do_not_matter() {
        assert_eq!(Route::parse("/Title/S7/"), Route::Title(ItemRef::Synth(7)));
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
