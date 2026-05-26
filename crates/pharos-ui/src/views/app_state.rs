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
use crate::client::AdminUser;
use crate::views::{
    AdminAction, AdminView, CreateUserAttempt, LibraryView, LoginAttempt, LoginForm,
    PlayerView,
};
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRoute {
    Library,
    Player {
        item_id: String,
        kind: ItemKind,
    },
    Admin,
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
    let is_admin = user.read().as_ref().map(|u| u.is_admin).unwrap_or(false);
    let current_user_id = user.read().as_ref().map(|u| u.id.clone()).unwrap_or_default();
    let access_token = user
        .read()
        .as_ref()
        .map(|u| u.access_token.clone())
        .unwrap_or_default();
    rsx! {
        nav {
            class: "pharos-nav",
            button {
                class: "pharos-nav-library",
                onclick: move |_| route.set(AppRoute::Library),
                "Library"
            }
            if is_admin {
                button {
                    class: "pharos-nav-admin",
                    onclick: move |_| route.set(AppRoute::Admin),
                    "Admin"
                }
            }
        }
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
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    on_back: move |_| { route.set(AppRoute::Library); },
                }
            },
            AppRoute::Admin => rsx! {
                AdminPane {
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    current_user_id: current_user_id.clone(),
                }
            }
        }
    }
}

#[component]
fn AdminPane(
    access_token: String,
    server_base: String,
    current_user_id: String,
) -> Element {
    let reload = use_signal(|| 0u32);
    let status = use_signal::<Option<String>>(|| None);
    let users_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_admin_users(&base, &token).await }
        })
    };

    let action_handler = {
        let access_token = access_token.clone();
        let server_base = server_base.clone();
        let mut reload_signal = reload;
        let mut status_signal = status;
        move |action: AdminAction| {
            let token = access_token.clone();
            let base = server_base.clone();
            spawn(async move {
                match action {
                    AdminAction::Refresh => {}
                    AdminAction::LibraryRefresh => {
                        match library_refresh(&base, &token).await {
                            Ok(()) => status_signal.set(Some("Library refresh broadcast".into())),
                            Err(e) => status_signal.set(Some(format!("Refresh failed: {e}"))),
                        }
                    }
                    AdminAction::CreateUser(CreateUserAttempt { name, password }) => {
                        match create_user(&base, &token, &name, &password).await {
                            Ok(()) => status_signal.set(Some(format!("Created {name}"))),
                            Err(e) => status_signal.set(Some(format!("Create failed: {e}"))),
                        }
                    }
                    AdminAction::DeleteUser(id) => {
                        match delete_user(&base, &token, &id).await {
                            Ok(()) => status_signal.set(Some(format!("Deleted {id}"))),
                            Err(e) => status_signal.set(Some(format!("Delete failed: {e}"))),
                        }
                    }
                }
                let n = *reload_signal.read();
                reload_signal.set(n.wrapping_add(1));
            });
        }
    };

    let value = users_resource.read_unchecked();
    let (users, fetch_err) = match value.as_ref() {
        None => (Vec::<AdminUser>::new(), Some("loading…".to_string())),
        Some(Ok(v)) => (v.clone(), None),
        Some(Err(e)) => (Vec::new(), Some(e.clone())),
    };
    let combined_status = fetch_err.or_else(|| status.read().clone());

    rsx! {
        AdminView {
            users: users,
            current_user_id: current_user_id,
            status: combined_status,
            on_action: action_handler,
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

#[cfg(feature = "web")]
async fn fetch_admin_users(base: &str, token: &str) -> Result<Vec<AdminUser>, String> {
    crate::client::web::admin_list_users(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_admin_users(_base: &str, _token: &str) -> Result<Vec<AdminUser>, String> {
    Err("admin user list is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn create_user(
    base: &str,
    token: &str,
    name: &str,
    password: &str,
) -> Result<(), String> {
    crate::client::web::admin_create_user(base, token, name, password)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn create_user(
    _base: &str,
    _token: &str,
    _name: &str,
    _password: &str,
) -> Result<(), String> {
    Err("create_user is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn delete_user(base: &str, token: &str, user_id: &str) -> Result<(), String> {
    crate::client::web::admin_delete_user(base, token, user_id)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn delete_user(_base: &str, _token: &str, _user_id: &str) -> Result<(), String> {
    Err("delete_user is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn library_refresh(base: &str, token: &str) -> Result<(), String> {
    crate::client::web::admin_library_refresh(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn library_refresh(_base: &str, _token: &str) -> Result<(), String> {
    Err("library_refresh is only wired in the web build".into())
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
