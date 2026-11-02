//! The native UI's routes (009 contracts/serving.md): one typed value per
//! URL, so a link cannot name a page that does not exist. Every first
//! segment is in `pharos_server::ui::UI_ROUTES`, which keeps the server
//! from claiming it; [`FIRST_SEGMENTS`] is checked against that list.

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
    /// No route matched; carries the path for the message.
    NotFound(String),
}

/// The first path segment of every route but `Home`, lowercase.
pub const FIRST_SEGMENTS: &[&str] = &["browse", "title", "watch", "signin", "manage"];

impl Route {
    /// The route for a URL path. Case-insensitive, as the server lowercases
    /// paths before routing; a trailing slash is ignored.
    pub fn parse(path: &str) -> Route {
        let lower = path.to_ascii_lowercase();
        let segments: Vec<&str> = lower.split('/').filter(|s| !s.is_empty()).collect();
        let id = |s: &str| s.parse::<ItemRef>().ok();
        match segments.as_slice() {
            [] => Route::Home,
            ["browse", s] => id(s).map_or_else(|| not_found(path), Route::Browse),
            ["title", s] => id(s).map_or_else(|| not_found(path), Route::Title),
            ["watch", s] => id(s).map_or_else(|| not_found(path), Route::Watch),
            ["signin"] => Route::SignIn,
            ["manage"] => Route::Manage,
            _ => not_found(path),
        }
    }

    /// The URL path for this route; `parse(route.path()) == route`.
    pub fn path(&self) -> String {
        match self {
            Route::Home => "/".into(),
            Route::Browse(id) => format!("/browse/{id}"),
            Route::Title(id) => format!("/title/{id}"),
            Route::Watch(id) => format!("/watch/{id}"),
            Route::SignIn => "/signin".into(),
            Route::Manage => "/manage".into(),
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
            Route::NotFound(_) => "not_found",
        }
    }
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

    /// Each route's first segment is listed, so the server-side check sees
    /// every one.
    #[test]
    fn first_segments_cover_every_route() {
        for route in [
            Route::Browse(ItemRef::Real(1)),
            Route::Title(ItemRef::Real(1)),
            Route::Watch(ItemRef::Real(1)),
            Route::SignIn,
            Route::Manage,
        ] {
            let path = route.path();
            let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
            assert!(FIRST_SEGMENTS.contains(&first), "{first}");
        }
    }
}
