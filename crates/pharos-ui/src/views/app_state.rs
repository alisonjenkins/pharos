//! Top-level routing + state.
//!
//! `RootApp` owns three signals:
//! - `user`: `Option<LoggedInUser>` — `None` shows the login screen.
//! - `route`: `AppRoute::{Library, Player(item_id)}` — drives the
//!   main pane once authenticated.
//! - `error`: optional surfacing text for the login form.
//!
//! Under `feature = "web"` the login submission calls into
//! `crate::client::web::authenticate`; without the feature the
//! component still renders (host-build / tests) but submitting does
//! nothing useful.

use crate::api_types::{ItemKind, LibraryItem, LoggedInUser};
use crate::views::{LibraryView, LoginAttempt, LoginForm, PlayerView};
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRoute {
    Library,
    Player {
        item_id: String,
        kind: ItemKind,
    },
}

#[component]
pub fn RootApp() -> Element {
    let user = use_signal::<Option<LoggedInUser>>(|| None);
    let route = use_signal::<AppRoute>(|| AppRoute::Library);
    let error = use_signal::<Option<String>>(|| None);

    rsx! {
        div {
            class: "pharos-app",
            header { class: "pharos-banner", h1 { "pharos" } }
            main {
                class: "pharos-main",
                match user.read().as_ref() {
                    None => rsx! { LoginGate { user: user, error: error } },
                    Some(_) => rsx! {
                        Authenticated {
                            user: user,
                            route: route,
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn LoginGate(user: Signal<Option<LoggedInUser>>, error: Signal<Option<String>>) -> Element {
    rsx! {
        LoginForm {
            error: error.read().clone(),
            on_submit: move |attempt: LoginAttempt| {
                let mut user = user;
                let mut error = error;
                spawn(async move {
                    match login_via_client(&attempt).await {
                        Ok(u) => {
                            user.set(Some(u));
                            error.set(None);
                        }
                        Err(e) => error.set(Some(e)),
                    }
                });
            }
        }
    }
}

#[component]
fn Authenticated(
    user: Signal<Option<LoggedInUser>>,
    route: Signal<AppRoute>,
) -> Element {
    let server_base = server_base_from_window();
    let items_resource = {
        let user_for_resource = user;
        use_resource(move || {
            let base = server_base.clone();
            async move {
                let token = user_for_resource
                    .read()
                    .as_ref()
                    .map(|u| u.access_token.clone())
                    .unwrap_or_default();
                fetch_library(&base, &token).await
            }
        })
    };

    let current_route = route.read().clone();
    rsx! {
        match current_route {
            AppRoute::Library => rsx! {
                LibraryPane {
                    items_resource: items_resource,
                    on_play: move |(id, kind): (String, ItemKind)| {
                        route.set(AppRoute::Player { item_id: id, kind });
                    }
                }
            },
            AppRoute::Player { item_id, kind } => rsx! {
                PlayerPane {
                    item_id: item_id.clone(),
                    kind: kind,
                    access_token: user.read().as_ref().map(|u| u.access_token.clone()).unwrap_or_default(),
                    server_base: server_base_from_window(),
                    on_back: move |_| { route.set(AppRoute::Library); },
                }
            }
        }
    }
}

#[component]
fn LibraryPane(
    items_resource: Resource<Result<Vec<LibraryItem>, String>>,
    on_play: EventHandler<(String, ItemKind)>,
) -> Element {
    let value = items_resource.read_unchecked();
    match value.as_ref() {
        None => rsx! { p { class: "pharos-loading", "Loading library…" } },
        Some(Err(e)) => rsx! { p { class: "pharos-error", "Library error: {e}" } },
        Some(Ok(items)) => {
            // Map from `LibraryItem` click (id only) to (id, kind) by walking
            // the snapshot. Cheap for phase-1 library sizes.
            let items_for_lookup = items.clone();
            rsx! {
                LibraryView {
                    items: items.clone(),
                    on_play: move |id: String| {
                        if let Some(it) = items_for_lookup.iter().find(|i| i.id == id) {
                            on_play.call((it.id.clone(), it.kind));
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn PlayerPane(
    item_id: String,
    kind: ItemKind,
    access_token: String,
    server_base: String,
    on_back: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            class: "pharos-player-pane",
            button {
                class: "pharos-back",
                onclick: move |_| on_back.call(()),
                "← Back"
            }
            PlayerView {
                item_id: item_id,
                kind: kind,
                access_token: access_token,
                server_base: server_base,
                on_event: move |_| {},
            }
        }
    }
}

#[cfg(feature = "web")]
fn server_base_from_window() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_else(|| String::from(""))
}

#[cfg(not(feature = "web"))]
fn server_base_from_window() -> String {
    String::new()
}

#[cfg(feature = "web")]
async fn login_via_client(attempt: &LoginAttempt) -> Result<LoggedInUser, String> {
    let base = server_base_from_window();
    crate::client::web::authenticate(&base, &attempt.username, &attempt.password)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn login_via_client(_attempt: &LoginAttempt) -> Result<LoggedInUser, String> {
    Err("login is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_library(base: &str, token: &str) -> Result<Vec<LibraryItem>, String> {
    crate::client::web::list_items(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_library(_base: &str, _token: &str) -> Result<Vec<LibraryItem>, String> {
    Err("library fetch is only wired in the web build".into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn route_value_semantics() {
        let a = AppRoute::Player {
            item_id: "1".into(),
            kind: ItemKind::Movie,
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, AppRoute::Library);
    }

    #[test]
    fn server_base_on_host_is_empty() {
        // Without the `web` feature the base resolves to empty string;
        // the WASM build replaces this via `web_sys::window`.
        assert!(server_base_from_window().is_empty());
    }

    #[test]
    fn root_app_module_exports_present() {
        fn _f() -> Element {
            RootApp()
        }
        let _ = _f;
    }
}
