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
use crate::client::{
    ActivityEntry, AdminUser, ApiKey, DeviceEntry, ItemChapter, ItemDetail, LibraryFolder,
    LiveChannel, LiveProgram, LogEntry, PluginEntry, RemoteSession, ScheduledTask, SearchHint,
    UserConfiguration,
};
use crate::views::server_picker::{load_saved_servers, save_servers};
use crate::views::{
    AdminAction, AdminTab, AdminView, CreateUserAttempt, DetailAction, ItemDetailView, LibraryView,
    LiveTvAction, LiveTvStatus, LiveTvView, LoginAttempt, LoginForm, PlayerView, PrefsAction,
    PrefsTab, PrefsView, RemoteAction, RemoteControlView, SavedServer, SearchStatus, SearchView,
    ServerPickerAction, ServerPickerView,
};
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRoute {
    Library,
    Detail { item_id: String },
    Player {
        item_id: String,
        kind: ItemKind,
        chapters: Vec<ItemChapter>,
        run_time_ticks: u64,
    },
    LivePlayer { channel_id: String },
    Admin,
    Search,
    LiveTv,
    Prefs(PrefsTab),
    Remote,
    SelectServer,
}

#[component]
pub fn RootApp() -> Element {
    let user = use_signal::<Option<LoggedInUser>>(|| None);
    let route = use_signal::<AppRoute>(|| AppRoute::Library);
    let error = use_signal::<Option<String>>(|| None);
    // Pre-login server-picker toggle. None: show LoginForm. Some: show
    // ServerPickerPane. Post-login the same flow is reachable via
    // `AppRoute::SelectServer`.
    let mut pre_login_picker = use_signal(|| false);

    rsx! {
        div {
            class: "pharos-app",
            header { class: "pharos-banner", h1 { "pharos" } }
            main {
                class: "pharos-main",
                match user.read().as_ref() {
                    None => if *pre_login_picker.read() {
                        rsx! {
                            ServerPickerPane {
                                on_done: move |_| pre_login_picker.set(false),
                            }
                        }
                    } else {
                        rsx! {
                            div {
                                class: "pharos-pre-login",
                                LoginGate { user: user, error: error }
                                button {
                                    class: "pharos-switch-server",
                                    onclick: move |_| pre_login_picker.set(true),
                                    "Switch server"
                                }
                                p {
                                    class: "pharos-active-server",
                                    "Connected to: {active_server_label()}"
                                }
                            }
                        }
                    },
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

/// Visible label for the current active server. Strips
/// `https?://` prefix so the chrome stays compact.
fn active_server_label() -> String {
    let url = server_base_from_window();
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string()
}

#[component]
fn Authenticated(user: Signal<Option<LoggedInUser>>, route: Signal<AppRoute>) -> Element {
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
    let current_user_id = user
        .read()
        .as_ref()
        .map(|u| u.id.clone())
        .unwrap_or_default();
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
            button {
                class: "pharos-nav-search",
                onclick: move |_| route.set(AppRoute::Search),
                "Search"
            }
            button {
                class: "pharos-nav-livetv",
                onclick: move |_| route.set(AppRoute::LiveTv),
                "Live TV"
            }
            button {
                class: "pharos-nav-prefs",
                onclick: move |_| route.set(AppRoute::Prefs(PrefsTab::Display)),
                "Preferences"
            }
            button {
                class: "pharos-nav-remote",
                onclick: move |_| route.set(AppRoute::Remote),
                "Remote"
            }
            button {
                class: "pharos-nav-server",
                onclick: move |_| route.set(AppRoute::SelectServer),
                "Server"
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
                    on_select: move |id: String| {
                        route.set(AppRoute::Detail { item_id: id });
                    }
                }
            },
            AppRoute::Detail { item_id } => rsx! {
                DetailPane {
                    item_id: item_id.clone(),
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    current_user_id: current_user_id.clone(),
                    on_play: move |args: (String, ItemKind, Vec<ItemChapter>, u64)| {
                        let (id, kind, chapters, run_time_ticks) = args;
                        route.set(AppRoute::Player {
                            item_id: id,
                            kind,
                            chapters,
                            run_time_ticks,
                        });
                    },
                    on_back: move |_| { route.set(AppRoute::Library); },
                }
            },
            AppRoute::Player {
                item_id,
                kind,
                chapters,
                run_time_ticks,
            } => rsx! {
                PlayerPane {
                    item_id: item_id.clone(),
                    kind: kind,
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    chapters: chapters.clone(),
                    run_time_ticks: run_time_ticks,
                    on_back: move |_| { route.set(AppRoute::Library); },
                }
            },
            AppRoute::Admin => rsx! {
                AdminPane {
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    current_user_id: current_user_id.clone(),
                }
            },
            AppRoute::Search => rsx! {
                SearchPane {
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    on_select: move |id: String| {
                        route.set(AppRoute::Detail { item_id: id });
                    }
                }
            },
            AppRoute::LiveTv => rsx! {
                LiveTvPane {
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    on_tune: move |id: String| {
                        route.set(AppRoute::LivePlayer { channel_id: id });
                    }
                }
            },
            AppRoute::LivePlayer { channel_id } => rsx! {
                LivePlayerPane {
                    channel_id: channel_id.clone(),
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    on_back: move |_| { route.set(AppRoute::LiveTv); },
                }
            },
            AppRoute::Prefs(tab) => rsx! {
                PrefsPane {
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                    current_user_id: current_user_id.clone(),
                    active_tab: tab,
                    on_select_tab: move |t: PrefsTab| { route.set(AppRoute::Prefs(t)); },
                }
            },
            AppRoute::Remote => rsx! {
                RemotePane {
                    access_token: access_token.clone(),
                    server_base: server_base_from_window(),
                }
            },
            AppRoute::SelectServer => rsx! {
                ServerPickerPane {
                    on_done: move |_| { route.set(AppRoute::Library); },
                }
            }
        }
    }
}

#[component]
fn SearchPane(
    access_token: String,
    server_base: String,
    on_select: EventHandler<String>,
) -> Element {
    let mut query = use_signal(String::new);
    let hits = use_signal::<Vec<SearchHint>>(Vec::new);
    let status = use_signal::<SearchStatus>(|| SearchStatus::Idle);

    let do_search = {
        let access_token = access_token.clone();
        let server_base = server_base.clone();
        let mut hits = hits;
        let mut status = status;
        move |term: String| {
            let token = access_token.clone();
            let base = server_base.clone();
            if term.trim().is_empty() {
                hits.set(Vec::new());
                status.set(SearchStatus::Idle);
                return;
            }
            status.set(SearchStatus::Loading);
            spawn(async move {
                match fetch_search_hints(&base, &token, &term).await {
                    Ok(v) if v.is_empty() => {
                        hits.set(Vec::new());
                        status.set(SearchStatus::Empty);
                    }
                    Ok(v) => {
                        hits.set(v);
                        status.set(SearchStatus::Idle);
                    }
                    Err(e) => status.set(SearchStatus::Error(e)),
                }
            });
        }
    };

    let q_for_render = query.read().clone();
    let hits_now = hits.read().clone();
    let status_now = status.read().clone();

    rsx! {
        SearchView {
            query: q_for_render,
            hits: hits_now,
            status: status_now,
            on_query: move |q: String| {
                query.set(q.clone());
                do_search.clone()(q);
            },
            on_play: move |id: String| {
                // Click on a search hit routes to detail; user
                // hits Play from there. Matches jellyfin-web flow.
                on_select.call(id);
            },
        }
    }
}

#[component]
fn DetailPane(
    item_id: String,
    access_token: String,
    server_base: String,
    current_user_id: String,
    on_play: EventHandler<(String, ItemKind, Vec<ItemChapter>, u64)>,
    on_back: EventHandler<()>,
) -> Element {
    let reload = use_signal(|| 0u32);
    let status = use_signal::<Option<String>>(|| None);
    let detail_resource = {
        let id = item_id.clone();
        let token = access_token.clone();
        let base = server_base.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let id = id.clone();
            let token = token.clone();
            let base = base.clone();
            async move { fetch_item_detail_via_client(&base, &token, &id).await }
        })
    };

    let action_handler = {
        let access_token = access_token.clone();
        let server_base = server_base.clone();
        let item_id_for_handler = item_id.clone();
        let mut reload_signal = reload;
        let mut status_signal = status;
        let on_back = on_back;
        let on_play = on_play;
        move |action: DetailAction| {
            let token = access_token.clone();
            let base = server_base.clone();
            let id = item_id_for_handler.clone();
            match action {
                DetailAction::Back => on_back.call(()),
                DetailAction::Play => {
                    // Read the latest fetched detail to learn the kind +
                    // pass chapters / total runtime through to the
                    // PlayerView so it can render the chapter strip.
                    let detail_snapshot = detail_resource
                        .read()
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .cloned();
                    let kind = detail_snapshot
                        .as_ref()
                        .map(|d| d.kind)
                        .unwrap_or(ItemKind::Movie);
                    let chapters = detail_snapshot
                        .as_ref()
                        .map(|d| d.chapters.clone())
                        .unwrap_or_default();
                    let run_time_ticks =
                        detail_snapshot.as_ref().map(|d| d.run_time_ticks).unwrap_or(0);
                    on_play.call((id, kind, chapters, run_time_ticks));
                }
                DetailAction::TogglePlayed => {
                    let played_now = detail_resource
                        .read()
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .map(|d| d.played)
                        .unwrap_or(false);
                    let user_id = current_user_id.clone();
                    spawn(async move {
                        match toggle_played(&base, &token, &user_id, &id, !played_now).await {
                            Ok(()) => {
                                status_signal.set(None);
                                let n = *reload_signal.read();
                                reload_signal.set(n.wrapping_add(1));
                            }
                            Err(e) => {
                                status_signal.set(Some(format!("Played toggle failed: {e}")));
                            }
                        }
                    });
                }
                DetailAction::ToggleFavorite => {
                    let fav_now = detail_resource
                        .read()
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .map(|d| d.is_favorite)
                        .unwrap_or(false);
                    let user_id = current_user_id.clone();
                    spawn(async move {
                        match toggle_favorite(&base, &token, &user_id, &id, !fav_now).await {
                            Ok(()) => {
                                status_signal.set(None);
                                let n = *reload_signal.read();
                                reload_signal.set(n.wrapping_add(1));
                            }
                            Err(e) => {
                                status_signal.set(Some(format!("Favourite toggle failed: {e}")));
                            }
                        }
                    });
                }
            }
        }
    };

    let value = detail_resource.read_unchecked();
    let (detail_opt, fetch_err) = match value.as_ref() {
        None => (None, Some("loading…".to_string())),
        Some(Ok(d)) => (Some(d.clone()), None),
        Some(Err(e)) => (None, Some(e.clone())),
    };
    let combined_status = fetch_err.or_else(|| status.read().clone());

    match detail_opt {
        Some(detail) => {
            let primary_image_url = if detail.has_primary_image {
                Some(format!(
                    "{server_base}/Items/{item_id}/Images/Primary?api_key={access_token}"
                ))
            } else {
                None
            };
            let backdrop_image_url = if detail.has_backdrop_image {
                Some(format!(
                    "{server_base}/Items/{item_id}/Images/Backdrop?api_key={access_token}"
                ))
            } else {
                None
            };
            // Cast portraits use the public `<img>` route; no api_key
            // needed (matches the Items/Images/Primary contract).
            let person_image_url_template = Some(format!(
                "{server_base}/Items/{{person_id}}/Images/Primary"
            ));
            rsx! {
                ItemDetailView {
                    detail: detail,
                    error: combined_status,
                    primary_image_url: primary_image_url,
                    backdrop_image_url: backdrop_image_url,
                    person_image_url_template: person_image_url_template,
                    on_action: action_handler,
                }
            }
        }
        None => rsx! {
            div {
                class: "pharos-detail-loading",
                button {
                    class: "pharos-detail-back",
                    onclick: move |_| on_back.call(()),
                    "← Back"
                }
                if let Some(s) = combined_status.as_ref() {
                    p { class: "pharos-error", "{s}" }
                } else {
                    p { "loading…" }
                }
            }
        },
    }
}

#[component]
fn AdminPane(access_token: String, server_base: String, current_user_id: String) -> Element {
    let reload = use_signal(|| 0u32);
    let status = use_signal::<Option<String>>(|| None);
    let active_tab = use_signal::<AdminTab>(AdminTab::default);
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
    let libraries_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_virtual_folders(&base, &token).await }
        })
    };
    let devices_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_devices(&base, &token).await }
        })
    };
    let activity_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_activity_entries(&base, &token).await }
        })
    };
    let scheduled_tasks_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_scheduled_tasks(&base, &token).await }
        })
    };
    let plugins_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_plugins(&base, &token).await }
        })
    };
    let logs_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_logs(&base, &token).await }
        })
    };
    let api_keys_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_api_keys(&base, &token).await }
        })
    };
    let new_api_key_secret = use_signal::<Option<String>>(|| None);

    let action_handler = {
        let access_token = access_token.clone();
        let server_base = server_base.clone();
        let mut reload_signal = reload;
        let mut status_signal = status;
        let mut active_tab_signal = active_tab;
        let mut new_secret_signal = new_api_key_secret;
        move |action: AdminAction| {
            let token = access_token.clone();
            let base = server_base.clone();
            if let AdminAction::SelectTab(t) = action {
                active_tab_signal.set(t);
                return;
            }
            spawn(async move {
                match action {
                    AdminAction::Refresh => {}
                    AdminAction::LibraryRefresh => match library_refresh(&base, &token).await {
                        Ok(()) => status_signal.set(Some("Library refresh broadcast".into())),
                        Err(e) => status_signal.set(Some(format!("Refresh failed: {e}"))),
                    },
                    AdminAction::CreateUser(CreateUserAttempt { name, password }) => {
                        match create_user(&base, &token, &name, &password).await {
                            Ok(()) => status_signal.set(Some(format!("Created {name}"))),
                            Err(e) => status_signal.set(Some(format!("Create failed: {e}"))),
                        }
                    }
                    AdminAction::DeleteUser(id) => match delete_user(&base, &token, &id).await {
                        Ok(()) => status_signal.set(Some(format!("Deleted {id}"))),
                        Err(e) => status_signal.set(Some(format!("Delete failed: {e}"))),
                    },
                    AdminAction::SetUserPolicy { user_id, is_admin } => {
                        match set_user_policy(&base, &token, &user_id, is_admin).await {
                            Ok(()) => status_signal.set(Some(format!(
                                "{user_id} is now {}",
                                if is_admin { "admin" } else { "non-admin" }
                            ))),
                            Err(e) => status_signal.set(Some(format!("Policy update failed: {e}"))),
                        }
                    }
                    AdminAction::ResetUserPassword {
                        user_id,
                        new_password,
                    } => match reset_user_password(&base, &token, &user_id, &new_password).await {
                        Ok(()) => status_signal.set(Some(format!("Password reset for {user_id}"))),
                        Err(e) => status_signal.set(Some(format!("Password reset failed: {e}"))),
                    },
                    AdminAction::CreateApiKey { app_name } => {
                        match create_api_key(&base, &token, &app_name).await {
                            Ok(secret) => {
                                new_secret_signal.set(Some(secret));
                                status_signal.set(Some(format!("Issued API key '{app_name}'")));
                            }
                            Err(e) => status_signal
                                .set(Some(format!("API-key creation failed: {e}"))),
                        }
                    }
                    AdminAction::RevokeApiKey { key_id } => {
                        match revoke_api_key(&base, &token, &key_id).await {
                            Ok(()) => {
                                new_secret_signal.set(None);
                                status_signal.set(Some(format!("Revoked {key_id}")));
                            }
                            Err(e) => {
                                status_signal.set(Some(format!("API-key revoke failed: {e}")))
                            }
                        }
                    }
                    AdminAction::SelectTab(_) => unreachable!("handled above"),
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
    let libraries: Vec<LibraryFolder> = match libraries_resource.read_unchecked().as_ref() {
        Some(Ok(v)) => v.clone(),
        _ => Vec::new(),
    };
    let devices: Vec<DeviceEntry> = match devices_resource.read_unchecked().as_ref() {
        Some(Ok(v)) => v.clone(),
        _ => Vec::new(),
    };
    let activity: Vec<ActivityEntry> = match activity_resource.read_unchecked().as_ref() {
        Some(Ok(v)) => v.clone(),
        _ => Vec::new(),
    };
    let scheduled_tasks: Vec<ScheduledTask> =
        match scheduled_tasks_resource.read_unchecked().as_ref() {
            Some(Ok(v)) => v.clone(),
            _ => Vec::new(),
        };
    let plugins: Vec<PluginEntry> = match plugins_resource.read_unchecked().as_ref() {
        Some(Ok(v)) => v.clone(),
        _ => Vec::new(),
    };
    let logs: Vec<LogEntry> = match logs_resource.read_unchecked().as_ref() {
        Some(Ok(v)) => v.clone(),
        _ => Vec::new(),
    };
    let api_keys: Vec<ApiKey> = match api_keys_resource.read_unchecked().as_ref() {
        Some(Ok(v)) => v.clone(),
        _ => Vec::new(),
    };
    let combined_status = fetch_err.or_else(|| status.read().clone());
    let tab_now = *active_tab.read();
    let new_secret_now = new_api_key_secret.read().clone();

    rsx! {
        AdminView {
            users: users,
            current_user_id: current_user_id,
            status: combined_status,
            on_action: action_handler,
            active_tab: tab_now,
            libraries: libraries,
            devices: devices,
            activity: activity,
            scheduled_tasks: scheduled_tasks,
            plugins: plugins,
            logs: logs,
            api_keys: api_keys,
            new_api_key_secret: new_secret_now,
        }
    }
}

#[component]
fn LibraryPane(
    items_resource: Resource<Result<Vec<LibraryItem>, String>>,
    on_select: EventHandler<String>,
) -> Element {
    let value = items_resource.read_unchecked();
    match value.as_ref() {
        None => rsx! { p { class: "pharos-loading", "Loading library…" } },
        Some(Err(e)) => rsx! { p { class: "pharos-error", "Library error: {e}" } },
        Some(Ok(items)) => {
            rsx! {
                LibraryView {
                    items: items.clone(),
                    on_play: move |id: String| on_select.call(id),
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
    chapters: Vec<ItemChapter>,
    run_time_ticks: u64,
    on_back: EventHandler<()>,
) -> Element {
    use crate::views::QualityOption;
    let mut max_bitrate = use_signal::<Option<u32>>(|| None);
    let media_dom_id = format!("pharos-media-{item_id}");

    let item_for_url = item_id.clone();
    let server_for_url = server_base.clone();
    let token_for_url = access_token.clone();
    let kind_for_url = kind;
    let current_bitrate = *max_bitrate.read();
    let src_override: Option<String> = current_bitrate.map(|b| match kind_for_url {
        ItemKind::Audio => format!(
            "{server_for_url}/Audio/{item_for_url}/universal?api_key={token_for_url}&MaxStreamingBitrate={b}"
        ),
        ItemKind::Movie | ItemKind::Episode => format!(
            "{server_for_url}/Videos/{item_for_url}/stream?api_key={token_for_url}&MaxStreamingBitrate={b}"
        ),
    });

    let quality_options = vec![
        QualityOption {
            label: "Auto".into(),
            max_bitrate: 0,
        },
        QualityOption {
            label: "1080p · 8 Mbps".into(),
            max_bitrate: 8_000_000,
        },
        QualityOption {
            label: "720p · 4 Mbps".into(),
            max_bitrate: 4_000_000,
        },
        QualityOption {
            label: "480p · 2 Mbps".into(),
            max_bitrate: 2_000_000,
        },
        QualityOption {
            label: "Audio-only · 320 Kbps".into(),
            max_bitrate: 320_000,
        },
    ];

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
                src_override: src_override,
                quality_options: quality_options,
                current_max_bitrate: current_bitrate,
                chapters: chapters,
                run_time_ticks: run_time_ticks,
                on_event: move |ev: crate::views::PlaybackEvent| match ev {
                    crate::views::PlaybackEvent::QualityChanged { max_bitrate: b } => {
                        // 0 = Auto = drop the override.
                        max_bitrate.set(if b == 0 { None } else { Some(b) });
                    }
                    crate::views::PlaybackEvent::ChapterSelected { position_seconds } => {
                        seek_media(&media_dom_id, position_seconds);
                    }
                    _ => {}
                },
            }
        }
    }
}

/// Seek the `<video>` / `<audio>` whose DOM id matches `media_dom_id`.
/// Web-only — host build no-ops.
#[cfg(feature = "web")]
fn seek_media(media_dom_id: &str, position_seconds: f64) {
    use wasm_bindgen::JsCast;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(el) = doc.get_element_by_id(media_dom_id) else {
        return;
    };
    if let Ok(media) = el.dyn_into::<web_sys::HtmlMediaElement>() {
        media.set_current_time(position_seconds);
    }
}

#[cfg(not(feature = "web"))]
fn seek_media(_media_dom_id: &str, _position_seconds: f64) {}

#[component]
fn RemotePane(access_token: String, server_base: String) -> Element {
    let reload = use_signal(|| 0u32);
    let status = use_signal::<Option<String>>(|| None);
    let sessions_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_sessions(&base, &token).await }
        })
    };

    let action_handler = {
        let access_token = access_token.clone();
        let server_base = server_base.clone();
        let mut reload_signal = reload;
        let mut status_signal = status;
        move |action: RemoteAction| {
            let token = access_token.clone();
            let base = server_base.clone();
            if matches!(action, RemoteAction::Refresh) {
                let n = *reload_signal.read();
                reload_signal.set(n.wrapping_add(1));
                return;
            }
            spawn(async move {
                let result = match action {
                    RemoteAction::PlayState {
                        session_id,
                        command,
                        arg,
                    } => send_playstate(&base, &token, &session_id, &command, arg).await,
                    RemoteAction::General {
                        session_id,
                        command,
                        arg,
                    } => send_general(&base, &token, &session_id, &command, arg).await,
                    RemoteAction::Refresh => unreachable!("handled above"),
                };
                match result {
                    Ok(()) => status_signal.set(None),
                    Err(e) => status_signal.set(Some(format!("Command failed: {e}"))),
                }
            });
        }
    };

    let value = sessions_resource.read_unchecked();
    let (sessions, fetch_err) = match value.as_ref() {
        None => (Vec::<RemoteSession>::new(), Some("loading…".to_string())),
        Some(Ok(v)) => (v.clone(), None),
        Some(Err(e)) => (Vec::new(), Some(e.clone())),
    };
    let combined_status = fetch_err.or_else(|| status.read().clone());

    rsx! {
        RemoteControlView {
            sessions: sessions,
            self_session_id: None,
            status: combined_status,
            on_action: action_handler,
        }
    }
}

#[component]
fn PrefsPane(
    access_token: String,
    server_base: String,
    current_user_id: String,
    active_tab: PrefsTab,
    on_select_tab: EventHandler<PrefsTab>,
) -> Element {
    let reload = use_signal(|| 0u32);
    let status = use_signal::<Option<String>>(|| None);
    let config_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_user_configuration(&base, &token).await }
        })
    };

    let action_handler = {
        let access_token = access_token.clone();
        let server_base = server_base.clone();
        let user_id_for_handler = current_user_id.clone();
        let mut reload_signal = reload;
        let mut status_signal = status;
        let on_select_tab = on_select_tab;
        move |action: PrefsAction| match action {
            PrefsAction::SelectTab(t) => on_select_tab.call(t),
            PrefsAction::Save(cfg) => {
                let token = access_token.clone();
                let base = server_base.clone();
                let user_id = user_id_for_handler.clone();
                spawn(async move {
                    match save_user_configuration(&base, &token, &user_id, &cfg).await {
                        Ok(()) => {
                            status_signal.set(Some("Saved.".into()));
                            let n = *reload_signal.read();
                            reload_signal.set(n.wrapping_add(1));
                        }
                        Err(e) => status_signal.set(Some(format!("Save failed: {e}"))),
                    }
                });
            }
        }
    };

    let value = config_resource.read_unchecked();
    let (config, fetch_err) = match value.as_ref() {
        None => (UserConfiguration::default(), Some("loading…".to_string())),
        Some(Ok(c)) => (c.clone(), None),
        Some(Err(e)) => (UserConfiguration::default(), Some(e.clone())),
    };
    let combined_status = fetch_err.or_else(|| status.read().clone());

    rsx! {
        PrefsView {
            config: config,
            active_tab: active_tab,
            status: combined_status,
            on_action: action_handler,
        }
    }
}

#[component]
fn LivePlayerPane(
    channel_id: String,
    access_token: String,
    server_base: String,
    on_back: EventHandler<()>,
) -> Element {
    let src = format!("{server_base}/LiveTv/Channels/{channel_id}/Stream?api_key={access_token}");
    rsx! {
        div {
            class: "pharos-player-pane pharos-livetv-player",
            button {
                class: "pharos-back",
                onclick: move |_| on_back.call(()),
                "← Back"
            }
            PlayerView {
                item_id: channel_id.clone(),
                kind: ItemKind::Movie,
                access_token: access_token,
                server_base: server_base,
                src_override: Some(src),
                on_event: move |_| {},
            }
        }
    }
}

#[component]
fn LiveTvPane(access_token: String, server_base: String, on_tune: EventHandler<String>) -> Element {
    let reload = use_signal(|| 0u32);
    let channels_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_live_channels(&base, &token).await }
        })
    };
    let programs_resource = {
        let base = server_base.clone();
        let token = access_token.clone();
        let reload_signal = reload;
        use_resource(move || {
            let _bust = reload_signal.read();
            let base = base.clone();
            let token = token.clone();
            async move { fetch_live_programs(&base, &token, 6).await }
        })
    };

    // `/livetv/channels/{id}/images/primary` is a public 302 to the
    // upstream M3U logo — no api_key needed (matches `<img>` semantics
    // where headers cannot be injected).
    let logo_url_template = Some(format!(
        "{server_base}/LiveTv/Channels/{{id}}/Images/Primary"
    ));

    let action_handler = {
        let mut reload_signal = reload;
        let on_tune = on_tune;
        move |action: LiveTvAction| match action {
            LiveTvAction::Tune { channel_id } => on_tune.call(channel_id),
            LiveTvAction::Refresh => {
                let n = *reload_signal.read();
                reload_signal.set(n.wrapping_add(1));
            }
        }
    };

    let ch_value = channels_resource.read_unchecked();
    let prog_value = programs_resource.read_unchecked();
    let (channels, ch_status) = match ch_value.as_ref() {
        None => (Vec::<LiveChannel>::new(), LiveTvStatus::Loading),
        Some(Err(e)) => (Vec::new(), LiveTvStatus::Error(e.clone())),
        Some(Ok(v)) if v.is_empty() => (Vec::new(), LiveTvStatus::Empty),
        Some(Ok(v)) => (v.clone(), LiveTvStatus::Idle),
    };
    let programs: Vec<LiveProgram> = match prog_value.as_ref() {
        Some(Ok(v)) => v.clone(),
        _ => Vec::new(),
    };

    rsx! {
        LiveTvView {
            channels: channels,
            programs: programs,
            status: ch_status,
            logo_url_template: logo_url_template,
            on_action: action_handler,
        }
    }
}

/// Manages the localStorage-backed server list + Select/Add/Forget
/// actions. T59 phase 2. On Add we fire `/System/Info/Public` against
/// the typed URL to mint a `SavedServer` entry (server_id, name).
/// Switching servers updates `pharos.active_server_url` then reloads
/// the page so every cached fetch / `use_resource` drops cleanly.
#[component]
fn ServerPickerPane(on_done: EventHandler<()>) -> Element {
    let saved = use_signal::<Vec<SavedServer>>(load_saved_servers);
    let status = use_signal::<Option<String>>(|| None);

    let action_handler = {
        let mut saved_sig = saved;
        let mut status_sig = status;
        let on_done = on_done;
        move |action: ServerPickerAction| match action {
            ServerPickerAction::Select(entry) => {
                set_active_server_url(&entry.base_url);
                // Persist last-used ordering: move picked entry to front.
                let mut list = saved_sig.read().clone();
                list.retain(|s| s.server_id != entry.server_id);
                list.insert(0, entry.clone());
                save_servers(&list);
                saved_sig.set(list);
                on_done.call(());
                reload_app();
            }
            ServerPickerAction::Forget(server_id) => {
                let mut list = saved_sig.read().clone();
                let forgetting_active = list
                    .iter()
                    .any(|s| s.server_id == server_id && s.base_url == server_base_from_window());
                list.retain(|s| s.server_id != server_id);
                save_servers(&list);
                saved_sig.set(list);
                if forgetting_active {
                    clear_active_server_url();
                }
                status_sig.set(Some(format!("Forgot {server_id}")));
            }
            ServerPickerAction::Add(url) => {
                let trimmed = url.trim().trim_end_matches('/').to_string();
                if trimmed.is_empty() {
                    status_sig.set(Some("URL must not be empty".into()));
                    return;
                }
                spawn(async move {
                    match fetch_server_identity(&trimmed).await {
                        Ok(entry) => {
                            let mut list = saved_sig.read().clone();
                            // Dedup by server_id; replace existing.
                            list.retain(|s| s.server_id != entry.server_id);
                            list.insert(0, entry.clone());
                            save_servers(&list);
                            saved_sig.set(list);
                            status_sig.set(Some(format!("Added {}", entry.name)));
                        }
                        Err(e) => {
                            status_sig.set(Some(format!("Couldn't reach {trimmed}: {e}")));
                        }
                    }
                });
            }
        }
    };

    let default_url = active_server_label();
    let saved_now = saved.read().clone();
    let status_now = status.read().clone();
    rsx! {
        ServerPickerView {
            saved: saved_now,
            default_url: default_url,
            status: status_now,
            on_action: action_handler,
        }
    }
}

/// Fetch `/System/Info/Public` for a candidate base URL. Returns a
/// `SavedServer` populated with the upstream server's `Id` + `ServerName`.
/// Used by `ServerPickerPane::Add` to validate the URL before saving.
#[cfg(feature = "web")]
async fn fetch_server_identity(base_url: &str) -> Result<SavedServer, String> {
    use gloo_net::http::Request;
    let resp = Request::get(&format!("{base_url}/System/Info/Public"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("status {}", resp.status()));
    }
    let bytes = resp.binary().await.map_err(|e| e.to_string())?;
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct PublicSystemInfo {
        #[serde(default)]
        id: String,
        #[serde(default)]
        server_name: String,
    }
    let parsed: PublicSystemInfo =
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let name = if parsed.server_name.is_empty() {
        base_url.to_string()
    } else {
        parsed.server_name
    };
    Ok(SavedServer {
        server_id: parsed.id,
        base_url: base_url.to_string(),
        name,
        last_user_name: String::new(),
    })
}

#[cfg(not(feature = "web"))]
async fn fetch_server_identity(_base_url: &str) -> Result<SavedServer, String> {
    Err("fetch_server_identity is only wired in the web build".into())
}

/// Resolve the active server's base URL. Reads the
/// `pharos.active_server_url` localStorage key first (set by
/// `ServerPickerPane::Select` / `Add`); falls back to the page's
/// origin so the single-server bootstrap flow keeps working. Host
/// builds (no browser) always resolve to empty.
#[cfg(feature = "web")]
fn server_base_from_window() -> String {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        if let Ok(Some(saved)) = storage.get_item("pharos.active_server_url") {
            if !saved.is_empty() {
                return saved;
            }
        }
    }
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_else(|| String::from(""))
}

#[cfg(not(feature = "web"))]
fn server_base_from_window() -> String {
    String::new()
}

#[cfg(feature = "web")]
fn set_active_server_url(url: &str) {
    let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else {
        return;
    };
    let _ = storage.set_item("pharos.active_server_url", url);
}

#[cfg(not(feature = "web"))]
fn set_active_server_url(_url: &str) {}

#[cfg(feature = "web")]
fn clear_active_server_url() {
    let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else {
        return;
    };
    let _ = storage.remove_item("pharos.active_server_url");
}

#[cfg(not(feature = "web"))]
fn clear_active_server_url() {}

/// Force a full page reload — the simplest way to drop every cached
/// fetch / `use_resource` after the active server changes. WASM-only;
/// host build is a no-op.
#[cfg(feature = "web")]
fn reload_app() {
    if let Some(loc) = web_sys::window().map(|w| w.location()) {
        let _ = loc.reload();
    }
}

#[cfg(not(feature = "web"))]
fn reload_app() {}

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
async fn create_user(base: &str, token: &str, name: &str, password: &str) -> Result<(), String> {
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

#[cfg(feature = "web")]
async fn fetch_item_detail_via_client(
    base: &str,
    token: &str,
    id: &str,
) -> Result<ItemDetail, String> {
    crate::client::web::fetch_item_detail(base, token, id)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_item_detail_via_client(
    _base: &str,
    _token: &str,
    _id: &str,
) -> Result<ItemDetail, String> {
    Err("item detail fetch is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn toggle_played(
    base: &str,
    token: &str,
    user_id: &str,
    item_id: &str,
    played: bool,
) -> Result<(), String> {
    crate::client::web::mark_played(base, token, user_id, item_id, played)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn toggle_played(
    _base: &str,
    _token: &str,
    _user_id: &str,
    _item_id: &str,
    _played: bool,
) -> Result<(), String> {
    Err("mark_played is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn toggle_favorite(
    base: &str,
    token: &str,
    user_id: &str,
    item_id: &str,
    favorite: bool,
) -> Result<(), String> {
    crate::client::web::mark_favorite(base, token, user_id, item_id, favorite)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn toggle_favorite(
    _base: &str,
    _token: &str,
    _user_id: &str,
    _item_id: &str,
    _favorite: bool,
) -> Result<(), String> {
    Err("mark_favorite is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_sessions(base: &str, token: &str) -> Result<Vec<RemoteSession>, String> {
    crate::client::web::list_sessions(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_sessions(_base: &str, _token: &str) -> Result<Vec<RemoteSession>, String> {
    Err("list_sessions is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn send_playstate(
    base: &str,
    token: &str,
    session_id: &str,
    command: &str,
    arg: serde_json::Value,
) -> Result<(), String> {
    crate::client::web::send_session_playstate(base, token, session_id, command, arg)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn send_playstate(
    _base: &str,
    _token: &str,
    _session_id: &str,
    _command: &str,
    _arg: serde_json::Value,
) -> Result<(), String> {
    Err("send_session_playstate is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn send_general(
    base: &str,
    token: &str,
    session_id: &str,
    command: &str,
    arg: serde_json::Value,
) -> Result<(), String> {
    crate::client::web::send_session_general(base, token, session_id, command, arg)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn send_general(
    _base: &str,
    _token: &str,
    _session_id: &str,
    _command: &str,
    _arg: serde_json::Value,
) -> Result<(), String> {
    Err("send_session_general is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_api_keys(base: &str, token: &str) -> Result<Vec<ApiKey>, String> {
    crate::client::web::list_api_keys(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_api_keys(_base: &str, _token: &str) -> Result<Vec<ApiKey>, String> {
    Err("list_api_keys is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn create_api_key(base: &str, token: &str, app_name: &str) -> Result<String, String> {
    crate::client::web::create_api_key(base, token, app_name)
        .await
        .map(|k| k.access_token)
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn create_api_key(_base: &str, _token: &str, _app_name: &str) -> Result<String, String> {
    Err("create_api_key is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn revoke_api_key(base: &str, token: &str, key_id: &str) -> Result<(), String> {
    crate::client::web::revoke_api_key(base, token, key_id)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn revoke_api_key(_base: &str, _token: &str, _key_id: &str) -> Result<(), String> {
    Err("revoke_api_key is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn set_user_policy(
    base: &str,
    token: &str,
    user_id: &str,
    is_admin: bool,
) -> Result<(), String> {
    crate::client::web::admin_set_user_policy(base, token, user_id, is_admin)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn set_user_policy(
    _base: &str,
    _token: &str,
    _user_id: &str,
    _is_admin: bool,
) -> Result<(), String> {
    Err("admin_set_user_policy is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn reset_user_password(
    base: &str,
    token: &str,
    user_id: &str,
    new_password: &str,
) -> Result<(), String> {
    crate::client::web::admin_reset_user_password(base, token, user_id, new_password)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn reset_user_password(
    _base: &str,
    _token: &str,
    _user_id: &str,
    _new_password: &str,
) -> Result<(), String> {
    Err("admin_reset_user_password is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_scheduled_tasks(base: &str, token: &str) -> Result<Vec<ScheduledTask>, String> {
    crate::client::web::list_scheduled_tasks(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_scheduled_tasks(
    _base: &str,
    _token: &str,
) -> Result<Vec<ScheduledTask>, String> {
    Err("list_scheduled_tasks is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_plugins(base: &str, token: &str) -> Result<Vec<PluginEntry>, String> {
    crate::client::web::list_plugins(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_plugins(_base: &str, _token: &str) -> Result<Vec<PluginEntry>, String> {
    Err("list_plugins is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_logs(base: &str, token: &str) -> Result<Vec<LogEntry>, String> {
    crate::client::web::list_logs(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_logs(_base: &str, _token: &str) -> Result<Vec<LogEntry>, String> {
    Err("list_logs is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_virtual_folders(base: &str, token: &str) -> Result<Vec<LibraryFolder>, String> {
    crate::client::web::list_virtual_folders(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_virtual_folders(_base: &str, _token: &str) -> Result<Vec<LibraryFolder>, String> {
    Err("list_virtual_folders is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_devices(base: &str, token: &str) -> Result<Vec<DeviceEntry>, String> {
    crate::client::web::list_devices(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_devices(_base: &str, _token: &str) -> Result<Vec<DeviceEntry>, String> {
    Err("list_devices is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_activity_entries(base: &str, token: &str) -> Result<Vec<ActivityEntry>, String> {
    crate::client::web::list_activity_entries(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_activity_entries(_base: &str, _token: &str) -> Result<Vec<ActivityEntry>, String> {
    Err("list_activity_entries is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_user_configuration(base: &str, token: &str) -> Result<UserConfiguration, String> {
    crate::client::web::fetch_user_configuration(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_user_configuration(_base: &str, _token: &str) -> Result<UserConfiguration, String> {
    Err("user_configuration fetch is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn save_user_configuration(
    base: &str,
    token: &str,
    user_id: &str,
    cfg: &UserConfiguration,
) -> Result<(), String> {
    crate::client::web::save_user_configuration(base, token, user_id, cfg)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn save_user_configuration(
    _base: &str,
    _token: &str,
    _user_id: &str,
    _cfg: &UserConfiguration,
) -> Result<(), String> {
    Err("save_user_configuration is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_live_channels(base: &str, token: &str) -> Result<Vec<LiveChannel>, String> {
    crate::client::web::live_channels(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_live_channels(_base: &str, _token: &str) -> Result<Vec<LiveChannel>, String> {
    Err("live_channels is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_live_programs(
    base: &str,
    token: &str,
    hours: u32,
) -> Result<Vec<LiveProgram>, String> {
    crate::client::web::live_programs(base, token, hours)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_live_programs(
    _base: &str,
    _token: &str,
    _hours: u32,
) -> Result<Vec<LiveProgram>, String> {
    Err("live_programs is only wired in the web build".into())
}

#[cfg(feature = "web")]
async fn fetch_search_hints(
    base: &str,
    token: &str,
    term: &str,
) -> Result<Vec<SearchHint>, String> {
    crate::client::web::search_hints(base, token, term)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_search_hints(
    _base: &str,
    _token: &str,
    _term: &str,
) -> Result<Vec<SearchHint>, String> {
    Err("search_hints is only wired in the web build".into())
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
            chapters: Vec::new(),
            run_time_ticks: 0,
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
