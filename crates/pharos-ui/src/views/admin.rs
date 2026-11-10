//! The Manage pages (T50, T58): the admin dashboard over the
//! `api::jellyfin::admin` endpoints. A sidebar of sections beside the one
//! shown, in the settings pages' frame, so Manage looks like the rest of the
//! app (B921). Pure: the parent fetches, and these views render under
//! `dioxus-ssr`.

use crate::client::{
    series_item_ref, FixDone, FlaggedMatch, HandFix, ShowCandidate, SplitFolder, SplitGroup,
};
use crate::client::{
    ActivityEntry, AdminUser, ApiKey, BrandingConfig, DamageRange, DamagedFile, DeepCheck,
    DeviceEntry, DuplicateFilm, FilmVersion, IntegrityReport, IntegrityStatus, LibraryFolder,
    LogEntry, PluginEntry, ScheduledTask, SearchHint, SeriesIdentity,
};
use dioxus::prelude::*;

/// What the user typed into the create-user form when they hit submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateUserAttempt {
    pub name: String,
    pub password: String,
}

/// Outbound events from `AdminView`. The parent decides which fetch
/// fires; this keeps the component pure for host tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminAction {
    DeleteUser(String),
    CreateUser(CreateUserAttempt),
    /// T50 phase 2 — flip the admin bit. `is_admin = false` against
    /// the only remaining admin is refused server-side.
    SetUserPolicy {
        user_id: String,
        is_admin: bool,
    },
    /// T50 phase 2 — admin-reset another user's password. Empty
    /// `new_password` is rejected on the parent.
    ResetUserPassword {
        user_id: String,
        new_password: String,
    },
    /// T58 phase 3 — issue a new API key (`/Auth/Keys?App=name`).
    CreateApiKey {
        app_name: String,
    },
    /// T58 phase 3 — revoke an API key by its server-assigned id.
    RevokeApiKey {
        key_id: String,
    },
    /// T65 / UI — save the branding form (ServerName + LoginDisclaimer
    /// + CustomCss) via POST /System/Configuration.
    SaveBranding(BrandingConfig),
    /// Queue a damaged file to be read again by the integrity check.
    RecheckDamaged {
        media_id: String,
    },
    /// Queue a full decode of one file; slow, and one file runs at a time.
    DeepCheck {
        media_id: String,
    },
    /// The "Deep check another file" box changed.
    DeepSearch {
        term: String,
    },
    LibraryRefresh,
    Refresh,
    SelectTab(AdminTab),
    /// The status line's Dismiss.
    DismissStatus,
    /// Search the providers for the show a group or flagged match is.
    IdentitySearch {
        series_key: String,
        name: String,
        year: Option<u32>,
    },
    /// Make a candidate the show for `series_key`.
    IdentityPick {
        series_key: String,
        provider: String,
        id: String,
    },
    /// Keep the flagged guess as the show.
    IdentityConfirm {
        series_key: String,
    },
    /// Send `paths` of `folder` to an existing group (`target_key`) or a
    /// new one (`target_name` and `target_year`).
    IdentityMove {
        folder: String,
        paths: Vec<String>,
        target_key: Option<String>,
        target_name: Option<String>,
        target_year: Option<u32>,
    },
    /// Undo a folder's split.
    IdentityKeepTogether {
        folder: String,
    },
    /// Remove a hand fix.
    IdentityRemove {
        id: i64,
    },
    /// Replace one device's audio direct-play deny list (T130).
    SetDeviceAudioDeny {
        device_id: String,
        codecs: Vec<String>,
    },
}

/// The bitstream audio formats a device can be told not to direct-play,
/// as (codec, label). These are the ones a TV or receiver claims and then
/// mis-decodes; other codecs already in a device's list are kept.
pub const DENIABLE_AUDIO_CODECS: [(&str, &str); 4] = [
    ("ac3", "Dolby Digital"),
    ("eac3", "Dolby Digital Plus"),
    ("truehd", "Dolby TrueHD"),
    ("dts", "DTS"),
];

/// `current` with `codec` added or taken out, sorted so the same set always
/// saves the same.
pub fn toggled_codecs(current: &[String], codec: &str) -> Vec<String> {
    let mut next: Vec<String> = current
        .iter()
        .filter(|c| !c.eq_ignore_ascii_case(codec))
        .cloned()
        .collect();
    if next.len() == current.len() {
        next.push(codec.to_owned());
    }
    next.sort();
    next
}

/// `devices` with each list this page saved since the last load put in place
/// of the loaded one, so a toggle made before the reload lands builds on it.
pub fn with_saved_audio_deny(
    devices: Vec<DeviceEntry>,
    saved: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<DeviceEntry> {
    devices
        .into_iter()
        .map(|mut d| {
            if let Some(list) = saved.get(&d.id) {
                d.no_direct_play_audio_codecs = list.clone();
            }
            d
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdminTab {
    #[default]
    Users,
    Libraries,
    Duplicates,
    Integrity,
    ShowIdentity,
    Devices,
    Activity,
    ScheduledTasks,
    Plugins,
    Logs,
    ApiKeys,
    Branding,
}

impl AdminTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Users => "Users",
            Self::Libraries => "Libraries",
            Self::Duplicates => "Duplicates",
            Self::Integrity => "Damaged files",
            Self::ShowIdentity => "Show identity",
            Self::Devices => "Devices",
            Self::Activity => "Activity",
            Self::ScheduledTasks => "Tasks",
            Self::Plugins => "Plugins",
            Self::Logs => "Logs",
            Self::ApiKeys => "API keys",
            Self::Branding => "Branding",
        }
    }
    fn class_suffix(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Libraries => "libraries",
            Self::Duplicates => "duplicates",
            Self::Integrity => "integrity",
            Self::ShowIdentity => "showidentity",
            Self::Devices => "devices",
            Self::Activity => "activity",
            Self::ScheduledTasks => "scheduledtasks",
            Self::Plugins => "plugins",
            Self::Logs => "logs",
            Self::ApiKeys => "apikeys",
            Self::Branding => "branding",
        }
    }
    /// One line under the page title on what the page is for.
    fn lede(self) -> &'static str {
        match self {
            Self::Users => "Who can sign in, and who can manage the server.",
            Self::Libraries => "The folders pharos watches for media.",
            Self::Duplicates => "Films held more than once, to compare before you remove a copy.",
            Self::Integrity => "Files the background check found damaged, and deep checks.",
            Self::ShowIdentity => {
                "Folders split into several shows, matches to check, and the fixes made by hand."
            }
            Self::Devices => "Every app and device that has signed in.",
            Self::Activity => "Recent sign-ins, playback and server events.",
            Self::ScheduledTasks => "Jobs the server runs on its own.",
            Self::Plugins => "Plugins the server has loaded.",
            Self::Logs => "The server's log files.",
            Self::ApiKeys => "Keys that let other apps use this server without a user.",
            Self::Branding => "The server's name and the words on the sign-in page.",
        }
    }
    /// The icon's SVG path, on the settings sidebar's 24-unit stroked grid.
    fn icon(self) -> &'static str {
        match self {
            Self::Users => "M9 11a4 4 0 1 0 0-8a4 4 0 0 0 0 8zM2 21c.8-3.6 3.6-6 7-6s6.2 2.4 7 6M16 3.5a4 4 0 0 1 0 7M18 15c2 .8 3.4 2.9 4 6",
            Self::Libraries => "M4 4h4v16H4zM10 4h4v16h-4zM16.5 4.5l3.8-1 4 15.5-3.8 1z",
            Self::Duplicates => "M8 8h12v12H8zM4 16V4h12",
            Self::Integrity => "M12 3l9 16H3zM12 10v4M12 17v.5",
            Self::ShowIdentity => "M4 5h16v11H4zM8 20h8M12 16v4M9 10l2 2l4-4",
            Self::Devices => "M3 5h18v12H3zM8 21h8M12 17v4",
            Self::Activity => "M3 12h4l3-8 4 16 3-8h4",
            Self::ScheduledTasks => "M12 21a9 9 0 1 0 0-18a9 9 0 0 0 0 18zM12 7v5l3 2",
            Self::Plugins => "M9 3v5M15 3v5M6 8h12v4a6 6 0 0 1-12 0zM12 18v3",
            Self::Logs => "M6 3h9l4 4v14H6zM14 3v5h5M9 12h7M9 16h7",
            Self::ApiKeys => "M8 15a4 4 0 1 0 0-8a4 4 0 0 0 0 8zM12 11h9M18 11v3M21 11v3",
            Self::Branding => "M4 20l4-1L19 8l-3-3L5 16zM14 7l3 3",
        }
    }
    pub fn all() -> [AdminTab; 12] {
        [
            Self::Users,
            Self::Libraries,
            Self::Duplicates,
            Self::Integrity,
            Self::ShowIdentity,
            Self::Devices,
            Self::Activity,
            Self::ScheduledTasks,
            Self::Plugins,
            Self::Logs,
            Self::ApiKeys,
            Self::Branding,
        ]
    }
}

#[derive(PartialEq, Clone, Props)]
pub struct AdminViewProps {
    pub users: Vec<AdminUser>,
    pub current_user_id: String,
    pub status: Option<String>,
    pub on_action: EventHandler<AdminAction>,
    #[props(default)]
    pub active_tab: AdminTab,
    /// The open tab's listing has not answered yet: it shows "Loading…"
    /// rather than an empty list.
    #[props(default)]
    pub loading: bool,
    #[props(default)]
    pub libraries: Vec<LibraryFolder>,
    #[props(default)]
    pub devices: Vec<DeviceEntry>,
    #[props(default)]
    pub activity: Vec<ActivityEntry>,
    #[props(default)]
    pub scheduled_tasks: Vec<ScheduledTask>,
    #[props(default)]
    pub plugins: Vec<PluginEntry>,
    #[props(default)]
    pub logs: Vec<LogEntry>,
    #[props(default)]
    pub api_keys: Vec<ApiKey>,
    /// T58 phase 3 — most-recently-created key's secret string. Surfaces
    /// the AccessToken once after the POST round-trip; the parent clears
    /// it after the user dismisses the banner. None hides the banner.
    #[props(default)]
    pub new_api_key_secret: Option<String>,
    /// T65 — current branding snapshot for the Branding tab.
    #[props(default)]
    pub branding: BrandingConfig,
    /// Films held more than once; `None` until the Duplicates tab's
    /// listing has answered.
    #[props(default)]
    pub duplicates: Option<Result<Vec<DuplicateFilm>, String>>,
    /// Integrity status and damaged files; `None` until the Damaged files
    /// tab's listing has answered.
    #[props(default)]
    pub integrity: Option<Result<IntegrityReport, String>>,
    /// What needs a look in the series identity; `None` until the Show
    /// identity tab's listing has answered.
    #[props(default)]
    pub series_identity: Option<Result<SeriesIdentity, String>>,
    /// The pick form's search: whose it is, and what it found.
    #[props(default)]
    pub identity_search: IdentitySearchState,
    /// A group just made by a move that still needs its show picked.
    #[props(default)]
    pub identity_prompt: Option<PickPrompt>,
    /// Files whose Re-check was just requested from this page.
    #[props(default)]
    pub integrity_queued: Vec<String>,
    /// Re-check failures by media id, shown on the row.
    #[props(default)]
    pub integrity_errors: Vec<(String, String)>,
    /// Files whose Deep check was just requested from this page.
    #[props(default)]
    pub deep_queued: Vec<String>,
    /// Deep check failures by media id, shown on the row.
    #[props(default)]
    pub deep_errors: Vec<(String, String)>,
    /// The "Deep check another file" box and its hits.
    #[props(default)]
    pub deep_search: DeepSearchState,
    /// The devices whose sound formats are being saved; their toggles wait.
    #[props(default)]
    pub device_saving: std::collections::BTreeSet<String>,
}

/// What the pick form shows for the group or match it was opened on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentitySearchState {
    /// The series key the search was made for; other forms stay empty.
    pub series_key: String,
    pub loading: bool,
    pub hits: Vec<ShowCandidate>,
    pub error: Option<String>,
}

/// The group a move just made in `folder`, by year: its show still has to
/// be picked before its episodes can be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickPrompt {
    pub folder: String,
    pub year: Option<u32>,
}

/// What the "Deep check another file" box shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeepSearchState {
    pub query: String,
    pub hits: Vec<SearchHint>,
    pub loading: bool,
    pub error: Option<String>,
}

const PANEL_ID: &str = "manage-panel";
const HEADING_ID: &str = "manage-heading";

#[component]
pub fn AdminView(props: AdminViewProps) -> Element {
    let active_tab = props.active_tab;
    let on_action = props.on_action;
    let loading = props.loading;
    rsx! {
        div {
            class: "pharos-settings pharos-manage pharos-admin",
            "data-tab": "{active_tab.class_suffix()}",
            nav { class: "pharos-settings-nav pharos-admin-tabs", aria_label: "Manage",
                p { class: "pharos-settings-title", "Manage" }
                ul {
                    for t in AdminTab::all() {
                        li { key: "{t.class_suffix()}",
                            button {
                                r#type: "button",
                                class: "pharos-settings-link pharos-admin-tab",
                                "data-tab": "{t.class_suffix()}",
                                aria_current: if t == active_tab { "page" },
                                aria_controls: PANEL_ID,
                                onclick: move |_| on_action.call(AdminAction::SelectTab(t)),
                                svg {
                                    class: "pharos-settings-icon",
                                    view_box: "0 0 24 24",
                                    "aria-hidden": "true",
                                    path { d: t.icon() }
                                }
                                span { "{t.label()}" }
                            }
                        }
                    }
                }
            }
            section {
                id: PANEL_ID,
                class: "pharos-settings-page pharos-manage-page pharos-admin-section pharos-admin-section-{active_tab.class_suffix()}",
                aria_labelledby: HEADING_ID,
                header { class: "pharos-manage-head",
                    div { class: "pharos-manage-title",
                        h2 { id: HEADING_ID, "{active_tab.label()}" }
                        p { class: "pharos-settings-lede", "{active_tab.lede()}" }
                    }
                    div { class: "pharos-manage-tools",
                        if active_tab == AdminTab::Libraries {
                            button {
                                r#type: "button",
                                class: "pharos-admin-refresh-library",
                                onclick: move |_| on_action.call(AdminAction::LibraryRefresh),
                                "Scan all libraries"
                            }
                        }
                        button {
                            r#type: "button",
                            class: "pharos-admin-reload",
                            title: "Load this page again",
                            onclick: move |_| on_action.call(AdminAction::Refresh),
                            "Reload"
                        }
                    }
                }
                if let Some(msg) = props.status.as_ref() {
                    div { class: "pharos-admin-status", role: "status",
                        p { "{msg}" }
                        button {
                            r#type: "button",
                            class: "pharos-manage-dismiss",
                            aria_label: "Dismiss",
                            onclick: move |_| on_action.call(AdminAction::DismissStatus),
                            "×"
                        }
                    }
                }
                if loading {
                    p { class: "pharos-loading", "Loading…" }
                } else {
                    match active_tab {
                        AdminTab::Users => rsx! {
                            UsersPane {
                                users: props.users.clone(),
                                current_user_id: props.current_user_id.clone(),
                                on_action,
                            }
                        },
                        AdminTab::Libraries => rsx! { LibrariesPane { libraries: props.libraries.clone() } },
                        AdminTab::Devices => rsx! {
                            DevicesPane {
                                devices: props.devices.clone(),
                                saving: props.device_saving.clone(),
                                on_action,
                            }
                        },
                        AdminTab::Activity => rsx! { ActivityPane { entries: props.activity.clone() } },
                        AdminTab::ScheduledTasks => rsx! { TasksPane { tasks: props.scheduled_tasks.clone() } },
                        AdminTab::Duplicates => rsx! {
                            DuplicatesSection { films: props.duplicates.clone() }
                        },
                        AdminTab::Integrity => rsx! {
                            IntegritySection {
                                report: props.integrity.clone(),
                                queued: props.integrity_queued.clone(),
                                errors: props.integrity_errors.clone(),
                                deep_queued: props.deep_queued.clone(),
                                deep_errors: props.deep_errors.clone(),
                                deep_search: props.deep_search.clone(),
                                on_action,
                            }
                        },
                        AdminTab::ShowIdentity => rsx! {
                            ShowIdentitySection {
                                identity: props.series_identity.clone(),
                                search: props.identity_search.clone(),
                                prompt: props.identity_prompt.clone(),
                                on_action,
                            }
                        },
                        AdminTab::Plugins => rsx! { PluginsPane { plugins: props.plugins.clone() } },
                        AdminTab::Logs => rsx! { LogsPane { logs: props.logs.clone() } },
                        AdminTab::ApiKeys => rsx! {
                            ApiKeysPane {
                                keys: props.api_keys.clone(),
                                new_secret: props.new_api_key_secret.clone(),
                                on_action,
                            }
                        },
                        AdminTab::Branding => rsx! {
                            BrandingPane { branding: props.branding.clone(), on_action }
                        },
                    }
                }
            }
        }
    }
}

/// `9 users · 5 admins`.
fn users_summary(users: &[AdminUser]) -> String {
    let admins = users.iter().filter(|u| u.is_admin).count();
    let noun = if users.len() == 1 { "user" } else { "users" };
    let admin_noun = if admins == 1 { "admin" } else { "admins" };
    format!("{} {noun} · {admins} {admin_noun}", users.len())
}

#[component]
fn UsersPane(
    users: Vec<AdminUser>,
    current_user_id: String,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let mut new_name = use_signal(String::new);
    let mut new_password = use_signal(String::new);
    let name_empty = new_name.read().trim().is_empty();
    rsx! {
        if users.is_empty() {
            p { class: "pharos-empty", "No users yet." }
        } else {
            p { class: "pharos-manage-count", "{users_summary(&users)}" }
            ul { class: "pharos-manage-cards pharos-admin-user-list",
                for user in users.iter().cloned() {
                    AdminUserRow {
                        key: "{user.id}",
                        is_self: user.id == current_user_id,
                        user,
                        on_action,
                    }
                }
            }
        }
        section { class: "pharos-settings-group pharos-manage-form-card", aria_labelledby: "manage-new-user",
            h3 { id: "manage-new-user", "Add a user" }
            form {
                class: "pharos-admin-create pharos-manage-form",
                onsubmit: move |ev: FormEvent| {
                    ev.prevent_default();
                    let name = new_name.read().trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let password = new_password.read().clone();
                    on_action.call(AdminAction::CreateUser(CreateUserAttempt { name, password }));
                    new_name.set(String::new());
                    new_password.set(String::new());
                },
                label { class: "pharos-manage-field",
                    span { "Name" }
                    input {
                        class: "pharos-admin-new-name",
                        r#type: "text",
                        autocomplete: "off",
                        required: true,
                        value: "{new_name.read()}",
                        oninput: move |ev| new_name.set(ev.value()),
                    }
                }
                label { class: "pharos-manage-field",
                    span { "Password" }
                    input {
                        class: "pharos-admin-new-password",
                        r#type: "password",
                        autocomplete: "new-password",
                        value: "{new_password.read()}",
                        oninput: move |ev| new_password.set(ev.value()),
                    }
                }
                button { r#type: "submit", disabled: name_empty, "Add user" }
            }
            p { class: "pharos-setting-help pharos-manage-form-help",
                "New users are not admins. Turn on Admin on their card to let them manage the server."
            }
        }
    }
}

/// What a user card has open below its actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserCardOpen {
    Nothing,
    ResetPassword,
    ConfirmDelete,
}

/// The element focus moves to when a user card goes from `from` to `to`.
fn focus_target(user_id: &str, from: UserCardOpen, to: UserCardOpen) -> Option<String> {
    match (from, to) {
        (_, UserCardOpen::ResetPassword) => Some(format!("manage-reset-{user_id}")),
        (_, UserCardOpen::ConfirmDelete) => Some(format!("manage-keep-{user_id}")),
        (UserCardOpen::ResetPassword, UserCardOpen::Nothing) => {
            Some(format!("manage-reset-open-{user_id}"))
        }
        (UserCardOpen::ConfirmDelete, UserCardOpen::Nothing) => {
            Some(format!("manage-delete-{user_id}"))
        }
        (UserCardOpen::Nothing, UserCardOpen::Nothing) => None,
    }
}

#[derive(PartialEq, Clone, Props)]
struct AdminUserRowProps {
    user: AdminUser,
    /// The signed-in admin's own card: no Delete, no admin switch (an admin
    /// cannot lock themselves out from here), and their own password is
    /// changed in Settings, which asks for the current one.
    is_self: bool,
    on_action: EventHandler<AdminAction>,
}

#[component]
fn AdminUserRow(props: AdminUserRowProps) -> Element {
    let mut open = use_signal(|| UserCardOpen::Nothing);
    let mut reset_pw = use_signal(String::new);
    let on_action = props.on_action;
    let user = props.user.clone();
    let is_admin = user.is_admin;
    let is_self = props.is_self;
    let id = user.id.clone();
    let name = user.name.clone();
    let switch_id = format!("manage-admin-{}", user.id);
    let id_for_policy = id.clone();
    let id_for_reset = id.clone();
    let id_for_delete = id.clone();
    let open_now = *open.read();
    // Opens or closes a panel, and focus goes with it: into what opened (so
    // Esc reaches it), back to the button that opened it on close.
    let id_for_focus = id.clone();
    let show = use_callback(move |next: UserCardOpen| {
        let from = *open.peek();
        open.set(next);
        if let Some(target) = focus_target(&id_for_focus, from, next) {
            crate::native::focus_after_render(target);
        }
    });
    rsx! {
        li {
            class: "pharos-manage-card pharos-admin-user",
            "data-user-id": "{id}",
            div { class: "pharos-manage-user-head",
                span { class: "pharos-avatar pharos-manage-avatar", "aria-hidden": "true",
                    {crate::native::account::initials(&name)}
                }
                div { class: "pharos-manage-user-who",
                    h3 { class: "pharos-admin-user-name", "{name}" }
                    p { class: "pharos-manage-badges",
                        if is_admin {
                            span { class: "pharos-manage-badge is-admin", "Admin" }
                        } else {
                            span { class: "pharos-manage-badge", "User" }
                        }
                        if is_self {
                            span { class: "pharos-manage-badge is-you", "You" }
                        }
                    }
                }
            }
            div { class: "pharos-manage-user-row pharos-admin-user-policy",
                span { id: "{switch_id}", "Admin" }
                button {
                    r#type: "button",
                    class: "pharos-switch",
                    role: "switch",
                    aria_checked: "{is_admin}",
                    aria_labelledby: "{switch_id}",
                    disabled: is_self,
                    title: if is_self { "You can't remove your own admin rights" },
                    onclick: move |_| on_action.call(AdminAction::SetUserPolicy {
                        user_id: id_for_policy.clone(),
                        is_admin: !is_admin,
                    }),
                }
            }
            if is_self {
                p { class: "pharos-setting-help",
                    "This is you. Change your own password in Settings, under Account."
                }
            } else {
                div { class: "pharos-manage-card-actions",
                    button {
                        r#type: "button",
                        id: "manage-reset-open-{id}",
                        class: "pharos-admin-user-reset-open",
                        aria_expanded: "{open_now == UserCardOpen::ResetPassword}",
                        onclick: move |_| {
                            reset_pw.set(String::new());
                            show(if open_now == UserCardOpen::ResetPassword {
                                UserCardOpen::Nothing
                            } else {
                                UserCardOpen::ResetPassword
                            });
                        },
                        "Reset password"
                    }
                    button {
                        r#type: "button",
                        id: "manage-delete-{id}",
                        class: "pharos-danger pharos-admin-user-delete",
                        aria_expanded: "{open_now == UserCardOpen::ConfirmDelete}",
                        onclick: move |_| show(if open_now == UserCardOpen::ConfirmDelete {
                            UserCardOpen::Nothing
                        } else {
                            UserCardOpen::ConfirmDelete
                        }),
                        "Delete"
                    }
                }
                match open_now {
                    UserCardOpen::Nothing => rsx! {},
                    UserCardOpen::ResetPassword => rsx! {
                        form {
                            class: "pharos-manage-inline pharos-admin-user-reset",
                            onsubmit: move |ev: FormEvent| {
                                ev.prevent_default();
                                let pw = reset_pw.read().clone();
                                if pw.is_empty() {
                                    return;
                                }
                                on_action.call(AdminAction::ResetUserPassword {
                                    user_id: id_for_reset.clone(),
                                    new_password: pw,
                                });
                                reset_pw.set(String::new());
                                show(UserCardOpen::Nothing);
                            },
                            onkeydown: move |ev: KeyboardEvent| {
                                if ev.key() == Key::Named(NamedKey::Escape) {
                                    show(UserCardOpen::Nothing);
                                }
                            },
                            label { class: "pharos-manage-field",
                                span { "New password for {name}" }
                                input {
                                    class: "pharos-admin-user-reset-input",
                                    r#type: "password",
                                    autocomplete: "new-password",
                                    id: "manage-reset-{id}",
                                    value: "{reset_pw.read()}",
                                    oninput: move |ev| reset_pw.set(ev.value()),
                                }
                            }
                            div { class: "pharos-manage-inline-actions",
                                button {
                                    r#type: "button",
                                    onclick: move |_| show(UserCardOpen::Nothing),
                                    "Cancel"
                                }
                                button {
                                    r#type: "submit",
                                    class: "pharos-admin-user-reset-submit",
                                    disabled: reset_pw.read().is_empty(),
                                    "Set password"
                                }
                            }
                        }
                    },
                    UserCardOpen::ConfirmDelete => rsx! {
                        div {
                            class: "pharos-manage-inline pharos-manage-confirm",
                            role: "group",
                            aria_label: "Confirm delete",
                            onkeydown: move |ev: KeyboardEvent| {
                                if ev.key() == Key::Named(NamedKey::Escape) {
                                    show(UserCardOpen::Nothing);
                                }
                            },
                            p {
                                "Delete {name}? Their sign-ins, watch history and settings go with them. This can't be undone."
                            }
                            div { class: "pharos-manage-inline-actions",
                                button {
                                    r#type: "button",
                                    id: "manage-keep-{id}",
                                    onclick: move |_| show(UserCardOpen::Nothing),
                                    "Keep {name}"
                                }
                                button {
                                    r#type: "button",
                                    class: "pharos-danger is-strong pharos-admin-user-delete-confirm",
                                    onclick: move |_| {
                                        open.set(UserCardOpen::Nothing);
                                        on_action.call(AdminAction::DeleteUser(id_for_delete.clone()));
                                    },
                                    "Delete {name}"
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// `movies` → `Films`, Jellyfin's collection types in the app's words.
fn library_kind(collection_type: &str) -> &str {
    match collection_type {
        "movies" => "Films",
        "tvshows" => "TV",
        "music" => "Music",
        "books" => "Books",
        "musicvideos" => "Music videos",
        "homevideos" => "Home videos",
        "boxsets" => "Collections",
        "" | "mixed" => "Mixed",
        other => other,
    }
}

#[component]
fn LibrariesPane(libraries: Vec<LibraryFolder>) -> Element {
    rsx! {
        if libraries.is_empty() {
            p { class: "pharos-empty", "No libraries configured" }
        } else {
            ul { class: "pharos-manage-cards pharos-admin-library-list",
                for lib in libraries.iter() {
                    li { class: "pharos-manage-card pharos-admin-library", key: "{lib.item_id}",
                        div { class: "pharos-manage-card-title",
                            h3 { class: "pharos-admin-library-name", "{lib.name}" }
                            span {
                                class: "pharos-manage-badge pharos-admin-library-kind",
                                "data-kind": "{lib.collection_type}",
                                "{library_kind(&lib.collection_type)}"
                            }
                        }
                        ul { class: "pharos-manage-paths pharos-admin-library-locations",
                            for loc in lib.locations.iter() {
                                li { key: "{loc}", code { "{loc}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn DevicesPane(
    devices: Vec<DeviceEntry>,
    saving: std::collections::BTreeSet<String>,
    on_action: EventHandler<AdminAction>,
) -> Element {
    rsx! {
        if devices.is_empty() {
            p { class: "pharos-empty", "No devices have connected" }
        } else {
            p { class: "pharos-manage-hint",
                "If a device plays a sound format out of sync or not at all, turn it off here. "
                "Pharos then converts that format for the device instead of sending it as it is."
            }
            div { class: "pharos-manage-table-wrap",
                table { class: "pharos-manage-table pharos-admin-device-table",
                    thead {
                        tr {
                            th { scope: "col", "Device" }
                            th { scope: "col", "App" }
                            th { scope: "col", "Last user" }
                            th { scope: "col", "Converted sound" }
                        }
                    }
                    tbody {
                        for d in devices.iter() {
                            tr { key: "{d.id}",
                                td { class: "pharos-manage-strong", "{d.name}" }
                                td { "{d.app_name}" }
                                td { "{d.last_user_name}" }
                                td {
                                    div {
                                        class: "pharos-manage-badges pharos-admin-device-audio",
                                        role: "group",
                                        aria_label: "Sound formats converted for {d.name}",
                                        "aria-busy": "{saving.contains(&d.id)}",
                                        for (codec, label) in DENIABLE_AUDIO_CODECS {
                                            {
                                                let denied = d
                                                    .no_direct_play_audio_codecs
                                                    .iter()
                                                    .any(|c| c.eq_ignore_ascii_case(codec));
                                                let busy = saving.contains(&d.id);
                                                let device_id = d.id.clone();
                                                let current = d.no_direct_play_audio_codecs.clone();
                                                rsx! {
                                                    button {
                                                        key: "{codec}",
                                                        r#type: "button",
                                                        class: "pharos-manage-badge pharos-admin-device-codec",
                                                        "aria-pressed": "{denied}",
                                                        disabled: busy,
                                                        onclick: move |_| {
                                                            on_action.call(AdminAction::SetDeviceAudioDeny {
                                                                device_id: device_id.clone(),
                                                                codecs: toggled_codecs(&current, codec),
                                                            })
                                                        },
                                                        "{label}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ActivityPane(entries: Vec<ActivityEntry>) -> Element {
    rsx! {
        if entries.is_empty() {
            p { class: "pharos-empty", "No activity recorded" }
        } else {
            ol { class: "pharos-manage-list pharos-admin-activity-list",
                for e in entries.iter() {
                    li { class: "pharos-manage-list-row pharos-admin-activity", key: "{e.id}",
                        span {
                            class: "pharos-manage-badge pharos-admin-activity-severity",
                            "data-severity": "{e.severity.to_ascii_lowercase()}",
                            "{e.severity}"
                        }
                        div { class: "pharos-manage-list-text",
                            span { class: "pharos-manage-strong pharos-admin-activity-name", "{e.name}" }
                            if !e.short_overview.is_empty() {
                                span { class: "pharos-admin-activity-overview", "{e.short_overview}" }
                            }
                        }
                        time {
                            class: "pharos-manage-when pharos-admin-activity-date",
                            datetime: "{e.date_iso}",
                            "{when_label(&e.date_iso)}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn TasksPane(tasks: Vec<ScheduledTask>) -> Element {
    rsx! {
        if tasks.is_empty() {
            p { class: "pharos-empty", "No scheduled tasks registered" }
        } else {
            div { class: "pharos-manage-table-wrap",
                table { class: "pharos-manage-table pharos-admin-tasks-table",
                    thead {
                        tr {
                            th { scope: "col", "Task" }
                            th { scope: "col", "Category" }
                            th { scope: "col", "State" }
                            th { scope: "col", "Last run" }
                        }
                    }
                    tbody {
                        for t in tasks.iter() {
                            tr { key: "{t.id}",
                                td { class: "pharos-manage-strong", "{t.name}" }
                                td { "{t.category}" }
                                td { class: "pharos-admin-task-state",
                                    span {
                                        class: "pharos-manage-badge",
                                        "data-state": "{t.state.to_ascii_lowercase()}",
                                        "{t.state}"
                                    }
                                }
                                td { class: "pharos-manage-when",
                                    time { datetime: "{t.last_execution_iso}", "{when_label(&t.last_execution_iso)}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn PluginsPane(plugins: Vec<PluginEntry>) -> Element {
    rsx! {
        if plugins.is_empty() {
            p { class: "pharos-empty", "No plugins installed" }
        } else {
            ul { class: "pharos-manage-list pharos-admin-plugin-list",
                for p in plugins.iter() {
                    li { class: "pharos-manage-list-row pharos-admin-plugin", key: "{p.id}",
                        div { class: "pharos-manage-list-text",
                            span { class: "pharos-manage-strong pharos-admin-plugin-name", "{p.name}" }
                            span { class: "pharos-admin-plugin-version", "Version {p.version}" }
                        }
                        if !p.status.is_empty() {
                            span { class: "pharos-manage-badge pharos-admin-plugin-status", "{p.status}" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn LogsPane(logs: Vec<LogEntry>) -> Element {
    rsx! {
        if logs.is_empty() {
            p { class: "pharos-empty", "No log files" }
        } else {
            div { class: "pharos-manage-table-wrap",
                table { class: "pharos-manage-table pharos-admin-log-list",
                    thead {
                        tr {
                            th { scope: "col", "File" }
                            th { scope: "col", "Size" }
                            th { scope: "col", "Modified" }
                        }
                    }
                    tbody {
                        for l in logs.iter() {
                            tr { class: "pharos-admin-log", key: "{l.name}",
                                td { class: "pharos-manage-strong pharos-admin-log-name", code { "{l.name}" } }
                                td { class: "pharos-admin-log-size", title: "{l.size_bytes} bytes", "{bytes_label(l.size_bytes)}" }
                                td { class: "pharos-manage-when pharos-admin-log-date",
                                    time { datetime: "{l.date_modified_iso}", "{when_label(&l.date_modified_iso)}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ApiKeysPane(
    keys: Vec<ApiKey>,
    new_secret: Option<String>,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let mut new_name = use_signal(String::new);
    let name_empty = new_name.read().trim().is_empty();
    rsx! {
        if let Some(secret) = new_secret.as_ref() {
            div { class: "pharos-manage-secret pharos-admin-apikey-new", role: "status",
                p { class: "pharos-manage-strong", "Copy this key now. It won't be shown again." }
                input {
                    class: "pharos-admin-apikey-value",
                    r#type: "text",
                    readonly: true,
                    aria_label: "New API key",
                    value: "{secret}",
                    onfocus: move |_| {
                        #[cfg(feature = "web")]
                        select_focused();
                    },
                }
            }
        }
        if keys.is_empty() {
            p { class: "pharos-empty", "No API keys issued" }
        } else {
            ul { class: "pharos-manage-list pharos-admin-apikey-list",
                for k in keys.iter().cloned() {
                    ApiKeyRow { key: "{k.id}", entry: k, on_action }
                }
            }
        }
        section { class: "pharos-settings-group pharos-manage-form-card", aria_labelledby: "manage-new-key",
            h3 { id: "manage-new-key", "Issue a key" }
            form {
                class: "pharos-admin-apikey-create pharos-manage-form",
                onsubmit: move |ev: FormEvent| {
                    ev.prevent_default();
                    let trimmed = new_name.read().trim().to_string();
                    if trimmed.is_empty() {
                        return;
                    }
                    on_action.call(AdminAction::CreateApiKey { app_name: trimmed });
                    new_name.set(String::new());
                },
                label { class: "pharos-manage-field",
                    span { "App name" }
                    input {
                        class: "pharos-admin-apikey-name",
                        r#type: "text",
                        autocomplete: "off",
                        value: "{new_name.read()}",
                        oninput: move |ev| new_name.set(ev.value()),
                    }
                }
                button {
                    class: "pharos-admin-apikey-create-submit",
                    r#type: "submit",
                    disabled: name_empty,
                    "Issue key"
                }
            }
        }
    }
}

/// Selects the text of the focused field, so the new key copies whole.
#[cfg(feature = "web")]
fn select_focused() {
    use wasm_bindgen::JsCast;
    if let Some(input) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element())
        .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
    {
        input.select();
    }
}

#[component]
fn BrandingPane(branding: BrandingConfig, on_action: EventHandler<AdminAction>) -> Element {
    let mut draft = use_signal(|| branding.clone());
    use_effect({
        let branding = branding.clone();
        move || {
            draft.set(branding.clone());
        }
    });
    let d = draft.read().clone();
    let changed = d != branding;
    rsx! {
        form {
            class: "pharos-settings-group pharos-manage-form-card pharos-admin-branding-form",
            onsubmit: move |ev: FormEvent| {
                ev.prevent_default();
                on_action.call(AdminAction::SaveBranding(draft.read().clone()));
            },
            label { class: "pharos-manage-field pharos-admin-branding-row",
                span { "Server name" }
                input {
                    class: "pharos-admin-branding-name",
                    r#type: "text",
                    value: "{d.server_name}",
                    oninput: move |ev| draft.write().server_name = ev.value(),
                }
            }
            label { class: "pharos-manage-field pharos-admin-branding-row",
                span { "Sign-in message" }
                small { class: "pharos-setting-help", "Shown under the sign-in form." }
                textarea {
                    class: "pharos-admin-branding-disclaimer",
                    rows: "3",
                    value: "{d.login_disclaimer}",
                    oninput: move |ev| draft.write().login_disclaimer = ev.value(),
                }
            }
            label { class: "pharos-manage-field pharos-admin-branding-row",
                span { "Custom CSS" }
                small { class: "pharos-setting-help", "Added to every page of the Jellyfin web client." }
                textarea {
                    class: "pharos-admin-branding-css",
                    rows: "8",
                    spellcheck: "false",
                    value: "{d.custom_css}",
                    oninput: move |ev| draft.write().custom_css = ev.value(),
                }
            }
            div { class: "pharos-manage-inline-actions",
                button {
                    r#type: "button",
                    disabled: !changed,
                    onclick: move |_| draft.set(branding.clone()),
                    "Discard changes"
                }
                button {
                    class: "pharos-admin-branding-save",
                    r#type: "submit",
                    disabled: !changed,
                    "Save"
                }
            }
        }
    }
}

#[component]
fn ApiKeyRow(entry: ApiKey, on_action: EventHandler<AdminAction>) -> Element {
    let mut confirming = use_signal(|| false);
    let id_for_revoke = entry.id.clone();
    let keep_id = format!("manage-revoke-keep-{}", entry.id);
    let revoke_id = format!("manage-revoke-{}", entry.id);
    let keep_id_for_open = keep_id.clone();
    let revoke_id_for_keep = revoke_id.clone();
    let revoke_id_for_esc = revoke_id.clone();
    rsx! {
        li { class: "pharos-manage-list-row pharos-admin-apikey", "data-apikey-id": "{entry.id}",
            div { class: "pharos-manage-list-text",
                span { class: "pharos-manage-strong pharos-admin-apikey-app", "{entry.app_name}" }
                if !entry.date_created_iso.is_empty() {
                    span { class: "pharos-admin-apikey-date",
                        "Issued "
                        time { datetime: "{entry.date_created_iso}", "{when_label(&entry.date_created_iso)}" }
                    }
                }
            }
            if confirming() {
                div {
                    class: "pharos-manage-inline-actions",
                    role: "group",
                    aria_label: "Confirm revoke",
                    onkeydown: move |ev: KeyboardEvent| {
                        if ev.key() == Key::Named(NamedKey::Escape) {
                            confirming.set(false);
                            crate::native::focus_after_render(revoke_id_for_esc.clone());
                        }
                    },
                    button {
                        r#type: "button",
                        id: "{keep_id}",
                        onclick: move |_| {
                            confirming.set(false);
                            crate::native::focus_after_render(revoke_id_for_keep.clone());
                        },
                        "Keep"
                    }
                    button {
                        r#type: "button",
                        class: "pharos-danger is-strong pharos-admin-apikey-revoke-confirm",
                        onclick: move |_| {
                            confirming.set(false);
                            on_action.call(AdminAction::RevokeApiKey { key_id: id_for_revoke.clone() });
                        },
                        "Revoke {entry.app_name}"
                    }
                }
            } else {
                button {
                    r#type: "button",
                    id: "{revoke_id}",
                    class: "pharos-danger pharos-admin-apikey-revoke",
                    onclick: move |_| {
                        confirming.set(true);
                        crate::native::focus_after_render(keep_id_for_open.clone());
                    },
                    "Revoke"
                }
            }
        }
    }
}

/// `2026-11-09 08:12` from an ISO-8601 timestamp, the UTC wall clock to the
/// minute; anything that is not one is shown as it came, an empty one as a
/// dash.
fn when_label(iso: &str) -> String {
    let iso = iso.trim();
    if iso.is_empty() || iso.starts_with("0001-01-01") {
        return UNKNOWN.to_string();
    }
    let bytes = iso.as_bytes();
    let shaped = bytes.len() >= 16
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && matches!(bytes[10], b'T' | b' ')
        && bytes[13] == b':';
    // `get` keeps a stray multi-byte character from panicking the render.
    match (shaped, iso.get(..10), iso.get(11..16)) {
        (true, Some(day), Some(time)) => format!("{day} {time}"),
        _ => iso.to_string(),
    }
}

/// A byte count in the largest unit that keeps it above one.
fn bytes_label(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Read-only on purpose: the admin compares versions here and removes
/// files outside pharos (V285).
#[component]
fn DuplicatesSection(films: Option<Result<Vec<DuplicateFilm>, String>>) -> Element {
    rsx! {
        section {
            class: "pharos-manage-stack pharos-admin-section-duplicates",
            match films {
                None => rsx! { p { class: "pharos-loading", "Looking for duplicates…" } },
                Some(Err(e)) => rsx! { p { class: "pharos-error", "Could not list duplicates: {e}" } },
                Some(Ok(films)) if films.is_empty() => rsx! {
                    p { class: "pharos-empty", "No film is in the library more than once." }
                },
                Some(Ok(films)) => rsx! {
                    p { class: "pharos-admin-duplicates-summary", "{duplicates_summary(&films)}" }
                    for film in films.iter() {
                        article {
                            class: "pharos-admin-duplicate",
                            key: "{film.versions.first().map(|v| v.id.as_str()).unwrap_or_default()}",
                            h4 {
                                "{film.title}"
                                if let Some(year) = film.production_year {
                                    span { class: "pharos-admin-duplicate-year", " ({year})" }
                                }
                            }
                            p { class: "pharos-admin-duplicate-match", "{match_label(film)}" }
                            table {
                                class: "pharos-admin-duplicate-versions",
                                thead {
                                    tr {
                                        th { "Path" }
                                        th { "Size" }
                                        th { "Resolution" }
                                        th { "Codec" }
                                        th { "Added" }
                                    }
                                }
                                tbody {
                                    for v in film.versions.iter() {
                                        tr {
                                            key: "{v.id}",
                                            td {
                                                class: "pharos-admin-duplicate-path",
                                                "{v.path}"
                                                if v.edition_count > 0 {
                                                    span {
                                                        class: "pharos-admin-duplicate-editions",
                                                        " + {editions_label(v.edition_count)}"
                                                    }
                                                }
                                            }
                                            td { "{size_label(v)}" }
                                            td { "{resolution_label(v)}" }
                                            td { "{codec_label(v)}" }
                                            td { "{added_label(v)}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn ShowIdentitySection(
    identity: Option<Result<SeriesIdentity, String>>,
    search: IdentitySearchState,
    prompt: Option<PickPrompt>,
    on_action: EventHandler<AdminAction>,
) -> Element {
    rsx! {
        section { class: "pharos-manage-stack pharos-admin-section-identity",
            match identity {
                None => rsx! { p { class: "pharos-loading", "Looking at the shows…" } },
                Some(Err(e)) => rsx! { p { class: "pharos-error", "Could not read the show identity: {e}" } },
                Some(Ok(identity)) => rsx! {
                    p { class: "pharos-admin-identity-summary", "{identity_summary(&identity)}" }
                    if !identity.splits.is_empty() {
                        h3 { "Split folders" }
                        p { class: "pharos-admin-identity-hint",
                            "One folder holds files for more than one show. Each group below becomes its own show."
                        }
                        for f in identity.splits.iter() {
                            SplitCard {
                                key: "{f.folder}",
                                folder: f.clone(),
                                repair_mode: identity.repair_mode.clone(),
                                search: search.clone(),
                                prompt: prompt.clone().filter(|p| p.folder == f.folder),
                                on_action,
                            }
                        }
                    }
                    if !identity.flagged.is_empty() {
                        h3 { "Matches to check" }
                        p { class: "pharos-admin-identity-hint",
                            "The matcher chose a show but another was nearly as good."
                        }
                        for m in identity.flagged.iter() {
                            FlaggedCard { key: "{m.series_key}", m: m.clone(), search: search.clone(), on_action }
                        }
                    }
                    if !identity.overrides.is_empty() {
                        h3 { "Hand fixes" }
                        p { class: "pharos-admin-identity-hint",
                            "Fixes made by hand, which the scanner keeps to."
                        }
                        for o in identity.overrides.iter() {
                            HandFixRow { key: "{o.id}", fix: o.clone(), on_action }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn SplitCard(
    folder: SplitFolder,
    repair_mode: String,
    search: IdentitySearchState,
    prompt: Option<PickPrompt>,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let status = split_status_label(&folder, &repair_mode);
    let mut ticked = use_signal(Vec::<String>::new);
    let mut picking = use_signal(|| None::<String>);
    // "" is the new group; anything else is a group's series key.
    let mut target = use_signal(String::new);
    let mut new_name = use_signal(|| folder_name(&folder.folder).to_owned());
    let mut new_year = use_signal(String::new);
    let ticked_now: Vec<String> = ticked
        .read()
        .iter()
        .filter(|p| folder.groups.iter().any(|g| g.paths.contains(p)))
        .cloned()
        .collect();
    let target_now = target.read().clone();
    let year_now = new_year.read().trim().to_owned();
    let new_year_value = year_now.parse::<u32>().ok();
    let new_group_ok = !new_name.read().trim().is_empty() && new_year_value.is_some();
    let can_move = !ticked_now.is_empty()
        && if target_now.is_empty() {
            new_group_ok
        } else {
            true
        };
    let prompted_key = prompt.as_ref().and_then(|p| {
        folder
            .groups
            .iter()
            .find(|g| g.year == p.year)
            .map(|g| g.series_key.clone())
    });
    let move_folder = folder.folder.clone();
    let keep_folder = folder.folder.clone();
    let move_paths = ticked_now.clone();
    rsx! {
        article {
            class: "pharos-admin-identity-card",
            "data-status": "{folder.status}",
            h4 { title: "{folder.folder}", "{folder_name(&folder.folder)}" }
            p { class: "pharos-admin-identity-status", "{status}" }
            for g in folder.groups.iter() {
                div { key: "{g.series_key}", class: "pharos-admin-identity-group",
                    p { class: "pharos-admin-identity-group-head",
                        "{group_label(g)}"
                        span { class: "pharos-admin-identity-count", " · {files_label(g.paths.len())}" }
                    }
                    if prompted_key.as_deref() == Some(g.series_key.as_str()) {
                        p { class: "pharos-admin-identity-prompt", role: "status", "{PICK_PROMPT}" }
                    }
                    ul { class: "pharos-admin-identity-files",
                        for path in g.paths.iter() {
                            li { key: "{path}",
                                label {
                                    input {
                                        r#type: "checkbox",
                                        checked: ticked.read().contains(path),
                                        onchange: {
                                            let path = path.clone();
                                            move |ev: FormEvent| {
                                                let mut list = ticked.write();
                                                list.retain(|p| *p != path);
                                                if ev.checked() {
                                                    list.push(path.clone());
                                                }
                                            }
                                        },
                                    }
                                    span { class: "pharos-admin-integrity-path", title: "{path}", " {folder_name(path)}" }
                                }
                            }
                        }
                    }
                    {
                        let key = g.series_key.clone();
                        let open = picking.read().as_deref() == Some(g.series_key.as_str())
                            || prompted_key.as_deref() == Some(g.series_key.as_str());
                        rsx! {
                            if open {
                                PickForm {
                                    series_key: key.clone(),
                                    default_name: folder_name(&folder.folder).to_owned(),
                                    default_year: g.year,
                                    search: search.clone(),
                                    on_action,
                                }
                            } else {
                                button {
                                    r#type: "button",
                                    class: "pharos-admin-identity-pick-open",
                                    onclick: move |_| picking.set(Some(key.clone())),
                                    "Pick the show…"
                                }
                            }
                        }
                    }
                }
            }
            if !folder.moves.is_empty() {
                details { class: "pharos-admin-identity-moves",
                    summary { "{files_label(folder.moves.len())} still to move" }
                    ul {
                        for m in folder.moves.iter() {
                            li { key: "{m.path}",
                                span { class: "pharos-admin-integrity-path", title: "{m.path}", "{folder_name(&m.path)}" }
                                " → {key_label(&m.to_key, m.to_year)}"
                            }
                        }
                    }
                }
            }
            if !folder.groups.is_empty() {
            form {
                class: "pharos-admin-identity-move pharos-manage-inline",
                onsubmit: move |ev: FormEvent| {
                    ev.prevent_default();
                    if !can_move {
                        return;
                    }
                    let to_new = target.read().is_empty();
                    on_action.call(AdminAction::IdentityMove {
                        folder: move_folder.clone(),
                        paths: move_paths.clone(),
                        target_key: (!to_new).then(|| target.read().clone()),
                        target_name: to_new.then(|| new_name.read().trim().to_owned()),
                        target_year: to_new.then_some(new_year_value).flatten(),
                    });
                    ticked.set(Vec::new());
                },
                label { class: "pharos-manage-field",
                    span { "Send ticked files to" }
                    select {
                        aria_label: "Send ticked files to",
                        onchange: move |ev| target.set(ev.value()),
                        for g in folder.groups.iter() {
                            option { key: "{g.series_key}", value: "{g.series_key}", selected: target_now == g.series_key, "{group_label(g)}" }
                        }
                        option { value: "", selected: target_now.is_empty(), "A new group…" }
                    }
                }
                if target_now.is_empty() {
                    label { class: "pharos-manage-field",
                        span { "Show name" }
                        input {
                            r#type: "text",
                            autocomplete: "off",
                            value: "{new_name.read()}",
                            oninput: move |ev| new_name.set(ev.value()),
                        }
                    }
                    label { class: "pharos-manage-field",
                        span { "Year" }
                        input {
                            r#type: "text",
                            inputmode: "numeric",
                            autocomplete: "off",
                            value: "{new_year.read()}",
                            oninput: move |ev| new_year.set(ev.value()),
                        }
                    }
                }
                button { r#type: "submit", disabled: !can_move,
                    "Move {files_label(ticked_now.len())}"
                }
            }
            }
            if folder.groups.len() > 1 {
                div { class: "pharos-manage-card-actions",
                    button {
                        r#type: "button",
                        class: "pharos-admin-identity-keep",
                        onclick: move |_| on_action.call(AdminAction::IdentityKeepTogether {
                            folder: keep_folder.clone(),
                        }),
                        "Keep together"
                    }
                }
            }
        }
    }
}

/// Search by name and optional year, then pick a candidate.
#[component]
fn PickForm(
    series_key: String,
    default_name: String,
    default_year: Option<u32>,
    search: IdentitySearchState,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let mut name = use_signal(|| default_name.clone());
    let mut year = use_signal(|| default_year.map(|y| y.to_string()).unwrap_or_default());
    let mine = search.series_key == series_key;
    let key_for_search = series_key.clone();
    let key_for_pick = series_key.clone();
    rsx! {
        div { class: "pharos-admin-identity-pick",
            form {
                class: "pharos-manage-inline",
                onsubmit: move |ev: FormEvent| {
                    ev.prevent_default();
                    let n = name.read().trim().to_owned();
                    if n.is_empty() {
                        return;
                    }
                    on_action.call(AdminAction::IdentitySearch {
                        series_key: key_for_search.clone(),
                        name: n,
                        year: year.read().trim().parse::<u32>().ok(),
                    });
                },
                label { class: "pharos-manage-field",
                    span { "Show name" }
                    input {
                        r#type: "text",
                        autocomplete: "off",
                        value: "{name.read()}",
                        oninput: move |ev| name.set(ev.value()),
                    }
                }
                label { class: "pharos-manage-field",
                    span { "Year (optional)" }
                    input {
                        r#type: "text",
                        inputmode: "numeric",
                        autocomplete: "off",
                        value: "{year.read()}",
                        oninput: move |ev| year.set(ev.value()),
                    }
                }
                button { r#type: "submit", "Search" }
            }
            if mine && search.loading {
                p { class: "pharos-loading", "Searching…" }
            }
            if mine {
                if let Some(e) = search.error.as_deref() {
                    p { class: "pharos-error", "Could not search: {e}" }
                }
            }
            if mine && !search.loading && search.error.is_none() && search.hits.is_empty() {
                p { class: "pharos-admin-identity-hint", "Search for the show by name to see candidates." }
            }
            if mine {
                ul { class: "pharos-admin-identity-hits",
                    for h in search.hits.iter() {
                        li { key: "{h.provider}-{h.id}",
                            span { "{candidate_label(h)}" }
                            button {
                                r#type: "button",
                                onclick: {
                                    let (provider, id) = (h.provider.clone(), h.id.clone());
                                    let key = key_for_pick.clone();
                                    move |_| on_action.call(AdminAction::IdentityPick {
                                        series_key: key.clone(),
                                        provider: provider.clone(),
                                        id: id.clone(),
                                    })
                                },
                                "This is the show"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn FlaggedCard(
    m: FlaggedMatch,
    search: IdentitySearchState,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let href =
        series_item_ref(&m.series_id).map(|id| crate::native::route::Route::Browse(id).path());
    let reason = flagged_reason(&m);
    let mut picking = use_signal(|| false);
    let confirm_key = m.series_key.clone();
    rsx! {
        article { class: "pharos-admin-identity-card",
            h4 {
                match href {
                    Some(href) => rsx! { a { href: "{href}", "{m.name}" } },
                    None => rsx! { "{m.name}" },
                }
            }
            p { class: "pharos-admin-identity-status", "{reason}" }
            p { class: "pharos-admin-identity-facts", "{flagged_facts(&m)}" }
            div { class: "pharos-manage-card-actions",
                button {
                    r#type: "button",
                    class: "pharos-admin-identity-confirm",
                    onclick: move |_| on_action.call(AdminAction::IdentityConfirm {
                        series_key: confirm_key.clone(),
                    }),
                    "Confirm this match"
                }
                if !*picking.read() {
                    button {
                        r#type: "button",
                        class: "pharos-admin-identity-pick-open",
                        onclick: move |_| picking.set(true),
                        "Pick another show…"
                    }
                }
            }
            if *picking.read() {
                PickForm {
                    series_key: m.series_key.clone(),
                    default_name: m.name.clone(),
                    default_year: None,
                    search: search.clone(),
                    on_action,
                }
            }
        }
    }
}

#[component]
fn HandFixRow(fix: HandFix, on_action: EventHandler<AdminAction>) -> Element {
    let id = fix.id;
    rsx! {
        article { class: "pharos-admin-identity-card",
            h4 { title: "{fix.folder}", "{folder_name(&fix.folder)}" }
            p { class: "pharos-admin-identity-status", "{hand_fix_words(&fix)}" }
            p { class: "pharos-admin-identity-facts", "{hand_fix_origin(&fix)}" }
            div { class: "pharos-manage-card-actions",
                button {
                    r#type: "button",
                    class: "pharos-admin-identity-remove",
                    onclick: move |_| on_action.call(AdminAction::IdentityRemove { id }),
                    "Remove"
                }
            }
        }
    }
}

/// Shown on a group a move just made until its show is picked.
pub const PICK_PROMPT: &str = "Pick the show for this group so its episodes can be rebuilt";

fn candidate_label(c: &ShowCandidate) -> String {
    match c.year {
        Some(y) => format!("{} ({y}) · {} {}", c.name, c.provider, c.id),
        None => format!("{} · {} {}", c.name, c.provider, c.id),
    }
}

/// What a fix did, for the status line: "Moved 3 files; rebuilding 12
/// episodes in the background."
pub fn fix_summary(done: &FixDone) -> String {
    let moved =
        (!done.moved.is_empty()).then(|| format!("Moved {}", files_label(done.moved.len())));
    let rebuilding = (done.rebuilding > 0).then(|| {
        let noun = if done.rebuilding == 1 {
            "episode"
        } else {
            "episodes"
        };
        format!("rebuilding {} {noun} in the background", done.rebuilding)
    });
    match (moved, rebuilding) {
        (Some(m), Some(r)) => format!("{m}; {r}."),
        (Some(m), None) => format!("{m}."),
        (None, Some(r)) => {
            let mut cap = r.chars();
            let first = cap.next().map(|c| c.to_uppercase().collect::<String>());
            format!("{}{}.", first.unwrap_or_default(), cap.as_str())
        }
        (None, None) => "Done; nothing needed rebuilding.".to_owned(),
    }
}

/// The separator between a folder and its year in a split series key.
const KEY_YEAR_SEP: char = '\u{1f}';

/// The last part of a path: a folder's own name. The full path goes in a
/// tooltip.
fn folder_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(path)
}

/// A series key as people say it: "Folder name (YEAR)". The key is the
/// folder, a unit separator and the year; `year` fills in a key without one.
fn key_label(key: &str, year: Option<u32>) -> String {
    let (folder, from_key) = match key.split_once(KEY_YEAR_SEP) {
        Some((folder, y)) => (folder, y.parse::<u32>().ok()),
        None => (key, None),
    };
    match from_key.or(year) {
        Some(y) => format!("{} ({y})", folder_name(folder)),
        None => folder_name(folder).to_owned(),
    }
}

fn group_label(g: &SplitGroup) -> String {
    key_label(&g.series_key, g.year)
}

fn files_label(n: usize) -> String {
    format!("{n} {}", if n == 1 { "file" } else { "files" })
}

/// `applied`, or `pending` with when the scanner will do it: after the
/// switch to apply under `dry_run`, at the next scan under `apply`.
fn split_status_label(folder: &SplitFolder, repair_mode: &str) -> String {
    if folder.status == "applied" {
        return "applied".to_owned();
    }
    match repair_mode {
        "dry_run" => "pending (dry run)".to_owned(),
        "apply" => "pending (next scan)".to_owned(),
        _ => "pending".to_owned(),
    }
}

fn flagged_reason(m: &FlaggedMatch) -> String {
    match (m.runner_up_id.as_deref(), m.runner_up_score) {
        (Some(id), Some(score)) => format!(
            "Close call: another show ({id}) scored {score:.2}, nearly as well as the match."
        ),
        (Some(id), None) => format!("Close call: another show ({id}) was nearly as good."),
        _ => "Close call between two shows.".to_owned(),
    }
}

fn flagged_facts(m: &FlaggedMatch) -> String {
    let mut parts = Vec::new();
    if let (Some(p), Some(id)) = (m.provider.as_deref(), m.external_id.as_deref()) {
        parts.push(format!("Matched to {p} {id}"));
    }
    if let Some(secs) = m.premiere_date {
        parts.push(format!("premiered {}", date_label(secs)));
    }
    parts.join(" · ")
}

fn hand_fix_words(fix: &HandFix) -> String {
    if fix.keeps_together {
        return "Kept together as one show.".to_owned();
    }
    let target = match fix.target_name.as_deref() {
        Some(name) => match fix.target_year {
            Some(y) => format!("{name} ({y})"),
            None => name.to_owned(),
        },
        None => key_label(&fix.target_key, fix.target_year),
    };
    match fix.path.as_deref() {
        Some(path) => format!("{} goes to {target}.", folder_name(path)),
        None => format!("The folder goes to {target}."),
    }
}

fn hand_fix_origin(fix: &HandFix) -> String {
    let who = match (fix.source.as_str(), fix.created_by.as_deref()) {
        (_, Some(user)) => format!("Set by {user}"),
        ("scanner", None) => "Set by the scanner".to_owned(),
        _ => "Set by an admin".to_owned(),
    };
    format!("{who} on {}", date_label(fix.created_at))
}

fn identity_summary(i: &SeriesIdentity) -> String {
    if i.splits.is_empty() && i.flagged.is_empty() && i.overrides.is_empty() {
        return "Nothing needs a look.".to_owned();
    }
    let pending = i.splits.iter().filter(|f| f.status != "applied").count();
    format!(
        "{} split {} ({pending} pending) · {} to check · {} by hand",
        i.splits.len(),
        if i.splits.len() == 1 {
            "folder"
        } else {
            "folders"
        },
        i.flagged.len(),
        i.overrides.len()
    )
}

/// No delete on purpose: the owner decides what to do with a damaged file
/// outside pharos, then asks for it to be checked again.
#[component]
fn IntegritySection(
    report: Option<Result<IntegrityReport, String>>,
    queued: Vec<String>,
    errors: Vec<(String, String)>,
    deep_queued: Vec<String>,
    deep_errors: Vec<(String, String)>,
    deep_search: DeepSearchState,
    on_action: EventHandler<AdminAction>,
) -> Element {
    rsx! {
        section {
            class: "pharos-manage-stack pharos-admin-section-integrity",
            match report {
                None => rsx! { p { class: "pharos-loading", "Checking on the files…" } },
                Some(Err(e)) => rsx! { p { class: "pharos-error", "Could not list damaged files: {e}" } },
                Some(Ok(report)) => rsx! {
                    p { class: "pharos-admin-integrity-summary", "{integrity_summary(&report.status)}" }
                    if !report.files.is_empty() {
                        a {
                            class: "pharos-button pharos-admin-integrity-export",
                            href: "{csv_data_url(&damaged_csv(&report.files))}",
                            download: "pharos-damaged-files.csv",
                            "Export CSV"
                        }
                    }
                    for f in report.files.iter() {
                        DamagedRow {
                            key: "{f.media_id}",
                            file: f.clone(),
                            queued: f.recheck_pending || queued.contains(&f.media_id),
                            error: errors.iter().find(|(id, _)| *id == f.media_id).map(|(_, e)| e.clone()),
                            deep_running: deep_queued.contains(&f.media_id)
                                || deep_in_progress(deep_list(&report), &f.media_id),
                            deep_error: deep_errors.iter().find(|(id, _)| *id == f.media_id).map(|(_, e)| e.clone()),
                            on_action: on_action,
                        }
                    }
                    DeepChecksSection {
                        checks: report.deep_checks.clone(),
                        queued: deep_queued.clone(),
                        errors: deep_errors.clone(),
                        search: deep_search.clone(),
                        on_action: on_action,
                    }
                },
            }
        }
    }
}

#[component]
fn DamagedRow(
    file: DamagedFile,
    queued: bool,
    error: Option<String>,
    deep_running: bool,
    deep_error: Option<String>,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let id = file.media_id.clone();
    let deep_id = file.media_id.clone();
    let can_deep = file.kind.as_deref() != Some("book");
    let breaks = if file.ranges.is_empty() {
        file.first_fault_ms.map(break_label)
    } else {
        Some(format!(
            "Damaged at {}",
            ranges_label(&file.ranges, ", ", 5)
        ))
    };
    rsx! {
        article {
            class: "pharos-admin-integrity-file",
            "data-verdict": "{file.verdict}",
            h4 {
                "{damaged_title(&file)}"
                span { class: "pharos-admin-integrity-kind", " · {kind_label(file.kind.as_deref())}" }
            }
            p {
                class: "pharos-admin-integrity-verdict",
                "{verdict_words(&file.verdict)}"
                if let Some(b) = breaks {
                    span { " · {b}" }
                }
            }
            if let Some(fault) = file.first_fault.as_deref() {
                p { class: "pharos-admin-integrity-fault", "{fault}" }
            }
            match file.path.as_deref() {
                Some(path) => rsx! { p { class: "pharos-admin-integrity-path", "{path}" } },
                None => rsx! {
                    p { class: "pharos-admin-integrity-path", "This file is no longer in the library." }
                },
            }
            if let Some(secs) = file.checked_at {
                p { class: "pharos-admin-integrity-checked", "Checked {date_label(secs)}" }
            }
            div { class: "pharos-manage-card-actions",
                if queued {
                    button { class: "pharos-admin-integrity-recheck", disabled: true, "Queued" }
                } else {
                    button {
                        class: "pharos-admin-integrity-recheck",
                        onclick: move |_| on_action.call(AdminAction::RecheckDamaged {
                            media_id: id.clone(),
                        }),
                        "Re-check"
                    }
                }
                if can_deep {
                    if deep_running {
                        button { class: "pharos-admin-integrity-deep", disabled: true, "Deep check running…" }
                    } else {
                        button {
                            class: "pharos-admin-integrity-deep",
                            onclick: move |_| on_action.call(AdminAction::DeepCheck {
                                media_id: deep_id.clone(),
                            }),
                            "Deep check"
                        }
                    }
                }
            }
            if let Some(e) = error {
                p { class: "pharos-error", "Could not queue the re-check: {e}" }
            }
            if let Some(e) = deep_error {
                p { class: "pharos-error", "Could not start the deep check: {e}" }
            }
        }
    }
}

#[component]
fn DeepChecksSection(
    checks: Result<Vec<DeepCheck>, String>,
    queued: Vec<String>,
    errors: Vec<(String, String)>,
    search: DeepSearchState,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let (checks, failure) = match checks {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    rsx! {
        section {
            class: "pharos-admin-integrity-deep-checks",
            h4 { "Deep checks" }
            div {
                class: "pharos-admin-integrity-deep-search",
                label {
                    "Deep check another file"
                    input {
                        class: "pharos-admin-integrity-deep-search-input",
                        r#type: "search",
                        placeholder: "Part of a title",
                        autocomplete: "off",
                        value: "{search.query}",
                        oninput: move |ev| on_action.call(AdminAction::DeepSearch { term: ev.value() }),
                    }
                }
                if search.loading {
                    p { class: "pharos-loading", "Searching…" }
                } else if let Some(e) = search.error.as_deref() {
                    p { class: "pharos-error", "Could not search: {e}" }
                } else if !search.query.trim().is_empty() && deep_candidates(&search.hits).is_empty() {
                    p { class: "pharos-admin-integrity-deep-empty", "No video or audio files match." }
                }
                ul {
                    class: "pharos-admin-integrity-deep-hits",
                    for h in deep_candidates(&search.hits) {
                        DeepHitRow {
                            key: "{h.id}",
                            hit: h.clone(),
                            running: queued.contains(&h.id) || deep_in_progress(&checks, &h.id),
                            error: errors.iter().find(|(id, _)| *id == h.id).map(|(_, e)| e.clone()),
                            on_action: on_action,
                        }
                    }
                }
            }
            if let Some(e) = failure {
                p { class: "pharos-error", "Could not list deep checks: {e}" }
            } else if checks.is_empty() {
                p {
                    class: "pharos-admin-integrity-deep-empty",
                    "A deep check decodes a whole file to find damage inside the picture or sound. It takes minutes and runs one file at a time, so use it on a file that plays badly."
                }
            }
            for c in checks.iter() {
                article {
                    key: "{c.media_id}-{c.requested_at}",
                    class: "pharos-admin-integrity-deep-check",
                    "data-state": "{c.state}",
                    h5 { "{deep_title(c)}" }
                    p {
                        class: "pharos-admin-integrity-verdict",
                        "{deep_state_words(c)}"
                        if let Some(ms) = c.first_error_ms {
                            span { " · {break_label(ms)}" }
                        }
                    }
                    if let Some(e) = c.first_error.as_deref() {
                        p { class: "pharos-admin-integrity-fault", "{e}" }
                    }
                    if let Some(line) = deep_timing(c) {
                        p { class: "pharos-admin-integrity-checked", "{line}" }
                    }
                }
            }
        }
    }
}

/// True while the file has a deep check waiting or decoding.
pub fn deep_in_progress(checks: &[DeepCheck], media_id: &str) -> bool {
    checks
        .iter()
        .any(|c| c.media_id == media_id && deep_is_active(c))
}

/// True while any deep check is waiting or decoding.
pub fn any_deep_active(checks: &[DeepCheck]) -> bool {
    checks.iter().any(deep_is_active)
}

/// Most hits the box offers at once.
const DEEP_SEARCH_LIMIT: usize = 8;

/// Hits a deep check can decode: video and audio only, never books, folders
/// or series.
fn deep_candidates(hits: &[SearchHint]) -> Vec<&SearchHint> {
    hits.iter()
        .filter(|h| matches!(h.media_type.as_str(), "Movie" | "Episode" | "Audio"))
        .take(DEEP_SEARCH_LIMIT)
        .collect()
}

#[component]
fn DeepHitRow(
    hit: SearchHint,
    running: bool,
    error: Option<String>,
    on_action: EventHandler<AdminAction>,
) -> Element {
    let id = hit.id.clone();
    rsx! {
        li {
            class: "pharos-admin-integrity-deep-hit",
            span { class: "pharos-admin-integrity-kind", "{hit.kind.label()} · " }
            span { "{hit.name}" }
            if running {
                button { class: "pharos-admin-integrity-deep", disabled: true, "Deep check running…" }
            } else {
                button {
                    class: "pharos-admin-integrity-deep",
                    onclick: move |_| on_action.call(AdminAction::DeepCheck {
                        media_id: id.clone(),
                    }),
                    "Deep check"
                }
            }
            if let Some(e) = error {
                p { class: "pharos-error", "Could not start the deep check: {e}" }
            }
        }
    }
}

/// The deep results, empty when the server could not list them.
fn deep_list(report: &IntegrityReport) -> &[DeepCheck] {
    report.deep_checks.as_ref().map_or(&[], Vec::as_slice)
}

/// Whether the tab should re-load itself: a deep check is waiting or decoding.
pub fn deep_poll_wanted(report: &Result<IntegrityReport, String>) -> bool {
    report.as_ref().is_ok_and(|r| any_deep_active(deep_list(r)))
}

/// A poll timer may re-load the tab only if no newer load has superseded it.
pub fn generation_current(scheduled: u32, current: u32) -> bool {
    scheduled == current
}

fn deep_is_active(c: &DeepCheck) -> bool {
    matches!(c.state.as_str(), "queued" | "running")
}

fn deep_state_words(c: &DeepCheck) -> String {
    match c.state.as_str() {
        "queued" => "Waiting".to_string(),
        "running" => "Decoding…".to_string(),
        "clean" => "No damage found".to_string(),
        "damaged" => {
            let noun = if c.errors == 1 { "error" } else { "errors" };
            format!("Damaged: {} decode {noun}", thousands(c.errors))
        }
        "failed" => match c.detail.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => format!("Couldn't run: {d}"),
            None => "Couldn't run".to_string(),
        },
        other => format!("Unknown state: {other}"),
    }
}

/// `Took 4m 12s · Finished 2026-11-04`, whichever parts the run has.
fn deep_timing(c: &DeepCheck) -> Option<String> {
    let parts: Vec<String> = [
        c.elapsed_secs
            .map(|s| format!("Took {}", duration_label(s))),
        c.finished_at.map(|s| format!("Finished {}", date_label(s))),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn duration_label(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3_600, secs % 3_600 / 60),
    }
}

fn integrity_summary(s: &IntegrityStatus) -> String {
    let complete = s.checked >= s.eligible;
    if s.damaged == 0 {
        return if complete {
            "Every file has been checked. Nothing is damaged.".to_string()
        } else {
            format!(
                "Nothing damaged so far ({} of {} files checked). Checking continues in the background.",
                thousands(s.checked),
                thousands(s.eligible)
            )
        };
    }
    let mut line = format!(
        "Checked {} of {} files · {} damaged",
        thousands(s.checked),
        thousands(s.eligible),
        thousands(s.damaged)
    );
    if s.queued > 0 {
        line.push_str(&format!(
            " · {} waiting to be re-checked",
            thousands(s.queued)
        ));
    }
    line.push('.');
    if !complete {
        line.push_str(" Checking continues in the background.");
    }
    line
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn file_name_of(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// `Series · S01E02 · Name` for an episode, the name for everything else,
/// the file name when the library has no name for it.
fn damaged_title(f: &DamagedFile) -> String {
    media_title(
        &f.media_id,
        f.kind.as_deref(),
        f.name.as_deref(),
        f.series_name.as_deref(),
        (f.season_number, f.episode_number),
        f.path.as_deref(),
    )
}

fn deep_title(c: &DeepCheck) -> String {
    media_title(
        &c.media_id,
        c.kind.as_deref(),
        c.name.as_deref(),
        c.series_name.as_deref(),
        (c.season_number, c.episode_number),
        c.path.as_deref(),
    )
}

fn media_title(
    media_id: &str,
    kind: Option<&str>,
    name: Option<&str>,
    series_name: Option<&str>,
    numbers: (Option<u32>, Option<u32>),
    path: Option<&str>,
) -> String {
    let named = name.filter(|n| !n.is_empty());
    let title = if kind == Some("episode") {
        let code = match numbers {
            (Some(s), Some(e)) => Some(format!("S{s:02}E{e:02}")),
            _ => None,
        };
        let parts: Vec<String> = [
            series_name.filter(|n| !n.is_empty()).map(str::to_string),
            code,
            named.map(str::to_string),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    } else {
        named.map(str::to_string)
    };
    title
        .or_else(|| path.map(|p| file_name_of(p).to_string()))
        .unwrap_or_else(|| format!("File {media_id}"))
}

fn kind_label(kind: Option<&str>) -> &'static str {
    match kind {
        Some("movie") => "Movie",
        Some("episode") => "Episode",
        Some("audio") => "Audio",
        Some("book") => "Book",
        _ => "File",
    }
}

fn verdict_words(verdict: &str) -> &'static str {
    match verdict {
        "short" => "Shorter than it claims",
        "unreadable" => "Can't be read",
        _ => "Damaged",
    }
}

/// `h:mm:ss`, hours unpadded.
fn hms(ms: u64) -> String {
    let total = ms / 1_000;
    format!(
        "{}:{:02}:{:02}",
        total / 3_600,
        total % 3_600 / 60,
        total % 60
    )
}

fn break_label(ms: u64) -> String {
    format!("Breaks at {}", hms(ms))
}

/// `0:10:00–0:19:00` per span, a lone time for a span a packet wide, at most
/// `max` spans then how many more.
fn ranges_label(ranges: &[DamageRange], sep: &str, max: usize) -> String {
    let mut parts: Vec<String> = ranges
        .iter()
        .take(max)
        .map(|r| {
            if r.end_ms / 1_000 > r.start_ms / 1_000 {
                format!("{}–{}", hms(r.start_ms), hms(r.end_ms))
            } else {
                hms(r.start_ms)
            }
        })
        .collect();
    if ranges.len() > max {
        parts.push(format!("{} more", ranges.len() - max));
    }
    parts.join(sep)
}

const DAMAGED_CSV_HEADER: [&str; 12] = [
    "Title",
    "Kind",
    "Series",
    "Season",
    "Episode",
    "Name",
    "Path",
    "Problem",
    "Damaged at",
    "Reader said",
    "Checked (UTC)",
    "Media id",
];

/// The damaged list as RFC 4180 CSV, one row per file, for finding each file
/// again to re-rip or re-download it.
pub(crate) fn damaged_csv(files: &[DamagedFile]) -> String {
    let mut out = String::new();
    push_csv_row(&mut out, DAMAGED_CSV_HEADER.iter().map(|h| h.to_string()));
    for f in files {
        let opt = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_default();
        let damaged_at = if f.ranges.is_empty() {
            f.first_fault_ms.map(hms).unwrap_or_default()
        } else {
            ranges_label(&f.ranges, "; ", usize::MAX)
        };
        push_csv_row(
            &mut out,
            [
                damaged_title(f),
                kind_label(f.kind.as_deref()).to_string(),
                f.series_name.clone().unwrap_or_default(),
                opt(f.season_number),
                opt(f.episode_number),
                f.name.clone().unwrap_or_default(),
                f.path.clone().unwrap_or_default(),
                verdict_words(&f.verdict).to_string(),
                damaged_at,
                f.first_fault.clone().unwrap_or_default(),
                f.checked_at.map(utc_label).unwrap_or_default(),
                f.media_id.clone(),
            ]
            .into_iter(),
        );
    }
    out
}

fn push_csv_row(out: &mut String, cells: impl Iterator<Item = String>) {
    for (i, cell) in cells.enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_cell(&cell));
    }
    out.push_str("\r\n");
}

/// Quoted when it holds a separator, quote or line break. A cell a
/// spreadsheet would run as a formula gets a leading `'`, since file names
/// are whatever the disk holds.
fn csv_cell(cell: &str) -> String {
    let guarded = if cell.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{cell}")
    } else {
        cell.to_string()
    };
    if guarded.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// A `data:` URL a download link can carry, with a BOM so a spreadsheet
/// opens the UTF-8 titles as UTF-8.
fn csv_data_url(csv: &str) -> String {
    let mut out = String::from("data:text/csv;charset=utf-8,%EF%BB%BF");
    for b in csv.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `2026-11-09T17:35:43Z`.
fn utc_label(secs: i64) -> String {
    let rem = secs.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        date_label(secs),
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

fn date_label(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

const UNKNOWN: &str = "—";

fn duplicates_summary(films: &[DuplicateFilm]) -> String {
    let files: usize = films.iter().map(|f| f.versions.len()).sum();
    let noun = if films.len() == 1 { "film" } else { "films" };
    format!("{} {noun} with {files} files between them.", films.len())
}

fn match_label(film: &DuplicateFilm) -> String {
    match film.provider_id.as_deref() {
        Some(id) => format!("Matched on {id}"),
        None => "Matched on title and year".to_string(),
    }
}

fn editions_label(n: usize) -> String {
    if n == 1 {
        "1 edition".to_string()
    } else {
        format!("{n} editions")
    }
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;
const KIB: f64 = 1024.0;

fn size_label(v: &FilmVersion) -> String {
    match v.size_bytes.map(|b| b as f64) {
        None => UNKNOWN.to_string(),
        Some(b) if b >= GIB => format!("{:.1} GiB", b / GIB),
        Some(b) if b >= MIB => format!("{:.0} MiB", b / MIB),
        Some(b) => format!("{:.0} KiB", b / KIB),
    }
}

/// `1080p` for the common frame sizes, `1920×800` for anything else.
fn resolution_label(v: &FilmVersion) -> String {
    match (v.width, v.height) {
        (Some(3840), Some(_)) => "2160p".to_string(),
        (Some(1920 | 1440), Some(1080)) => "1080p".to_string(),
        (Some(1280), Some(720)) => "720p".to_string(),
        (Some(w), Some(h)) => format!("{w}×{h}"),
        _ => UNKNOWN.to_string(),
    }
}

fn codec_label(v: &FilmVersion) -> String {
    v.video_codec.clone().unwrap_or_else(|| UNKNOWN.to_string())
}

/// The UTC calendar date of a Unix-seconds timestamp.
fn added_label(v: &FilmVersion) -> String {
    let Some(secs) = v.date_added else {
        return UNKNOWN.to_string();
    };
    date_label(secs)
}

/// Howard Hinnant's days-to-civil: days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::api_types::ItemKind;

    #[test]
    fn admin_view_module_exports_present() {
        fn _f(p: AdminViewProps) -> Element {
            AdminView(p)
        }
        let _ = _f;
    }

    #[test]
    fn times_show_the_minute_and_odd_values_pass_through() {
        assert_eq!(when_label("2026-11-09T08:12:44.123Z"), "2026-11-09 08:12");
        assert_eq!(when_label("2026-11-09 08:12:44"), "2026-11-09 08:12");
        assert_eq!(when_label(""), "—");
        assert_eq!(when_label("0001-01-01T00:00:00Z"), "—");
        assert_eq!(when_label("yesterday"), "yesterday");
        assert_eq!(when_label("2026-11-09T08:1€:00"), "2026-11-09T08:1€:00");
    }

    #[test]
    fn the_damaged_csv_names_each_file_and_where_it_breaks() {
        let files = vec![
            DamagedFile {
                media_id: "123".into(),
                path: Some("/tv/Show, The/S01E02.mkv".into()),
                verdict: "damaged".into(),
                first_fault: Some("0x00 \"invalid\" byte".into()),
                kind: Some("episode".into()),
                name: Some("Pilot".into()),
                series_name: Some("Show, The".into()),
                season_number: Some(1),
                episode_number: Some(2),
                checked_at: Some(1_794_245_743),
                ranges: vec![
                    DamageRange {
                        start_ms: 600_000,
                        end_ms: 1_140_000,
                    },
                    DamageRange {
                        start_ms: 3_723_000,
                        end_ms: 3_723_040,
                    },
                ],
                ..Default::default()
            },
            DamagedFile {
                media_id: "9".into(),
                path: Some("=HYPERLINK(\"x\").mkv".into()),
                verdict: "short".into(),
                first_fault_ms: Some(5_000),
                ..Default::default()
            },
        ];
        let csv = damaged_csv(&files);
        let lines: Vec<&str> = csv.split("\r\n").collect();
        assert_eq!(
            lines[0],
            "Title,Kind,Series,Season,Episode,Name,Path,Problem,Damaged at,Reader said,Checked (UTC),Media id"
        );
        assert!(
            lines[1].contains(r#""/tv/Show, The/S01E02.mkv""#),
            "a comma is quoted: {}",
            lines[1]
        );
        assert!(lines[1].contains(",1,2,Pilot,"), "{}", lines[1]);
        assert!(
            lines[1].contains("0:10:00–0:19:00; 1:02:03"),
            "every span, a packet-wide one as one time: {}",
            lines[1]
        );
        assert!(
            lines[1].contains(r#""0x00 ""invalid"" byte""#),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].ends_with(",2026-11-09T17:35:43Z,123"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains(r#""'=HYPERLINK(""x"").mkv""#),
            "a formula is defused: {}",
            lines[2]
        );
        assert!(
            lines[2].contains(",0:00:05,"),
            "no spans falls back to the first fault: {}",
            lines[2]
        );
        assert_eq!(lines.len(), 4, "one row per file and a final line end");

        let url = csv_data_url("a,b\r\n");
        assert_eq!(url, "data:text/csv;charset=utf-8,%EF%BB%BFa%2Cb%0D%0A");
    }

    #[test]
    fn a_long_list_of_spans_is_cut_short_on_screen() {
        let spans: Vec<DamageRange> = (0..8)
            .map(|i| DamageRange {
                start_ms: i * 60_000,
                end_ms: i * 60_000 + 5_000,
            })
            .collect();
        assert_eq!(
            ranges_label(&spans, ", ", 2),
            "0:00:00–0:00:05, 0:01:00–0:01:05, 6 more"
        );
    }

    #[test]
    fn byte_counts_pick_a_unit() {
        assert_eq!(bytes_label(512), "512 B");
        assert_eq!(bytes_label(12_345), "12 KiB");
        assert_eq!(bytes_label(5 * 1024 * 1024 + 1024 * 512), "5.5 MiB");
        assert_eq!(bytes_label(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn libraries_and_user_counts_read_plainly() {
        assert_eq!(library_kind("movies"), "Films");
        assert_eq!(library_kind("tvshows"), "TV");
        assert_eq!(library_kind(""), "Mixed");
        assert_eq!(library_kind("podcasts"), "podcasts");
        let user = |admin: bool| AdminUser {
            id: "u".into(),
            name: "u".into(),
            is_admin: admin,
        };
        assert_eq!(users_summary(&[user(true)]), "1 user · 1 admin");
        assert_eq!(
            users_summary(&[user(true), user(false), user(true)]),
            "3 users · 2 admins"
        );
    }

    #[test]
    fn focus_follows_what_a_user_card_opens_and_returns_on_close() {
        use UserCardOpen::*;
        let at = |from, to| focus_target("u1", from, to);
        assert_eq!(
            at(Nothing, ConfirmDelete).as_deref(),
            Some("manage-keep-u1")
        );
        assert_eq!(
            at(Nothing, ResetPassword).as_deref(),
            Some("manage-reset-u1")
        );
        assert_eq!(
            at(ConfirmDelete, Nothing).as_deref(),
            Some("manage-delete-u1")
        );
        assert_eq!(
            at(ResetPassword, Nothing).as_deref(),
            Some("manage-reset-open-u1")
        );
        assert_eq!(at(Nothing, Nothing), None);
    }

    #[test]
    fn every_tab_has_a_lede_and_an_icon() {
        for tab in AdminTab::all() {
            assert!(!tab.lede().is_empty(), "{tab:?}");
            assert!(!tab.icon().is_empty(), "{tab:?}");
        }
    }

    #[test]
    fn toggling_a_codec_adds_or_removes_it_and_keeps_the_rest() {
        let own = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(toggled_codecs(&[], "eac3"), own(&["eac3"]));
        assert_eq!(
            toggled_codecs(&own(&["opus", "eac3"]), "EAC3"),
            own(&["opus"])
        );
        assert_eq!(
            toggled_codecs(&own(&["opus"]), "ac3"),
            own(&["ac3", "opus"])
        );
    }

    #[test]
    fn a_saved_list_replaces_the_loaded_one_until_the_next_load() {
        let loaded = vec![
            DeviceEntry {
                id: "tv".into(),
                ..Default::default()
            },
            DeviceEntry {
                id: "pc".into(),
                no_direct_play_audio_codecs: vec!["dts".into()],
                ..Default::default()
            },
        ];
        let saved = std::collections::HashMap::from([("tv".to_string(), vec!["eac3".to_string()])]);
        let merged = with_saved_audio_deny(loaded, &saved);
        assert_eq!(
            merged[0].no_direct_play_audio_codecs,
            vec!["eac3".to_string()]
        );
        assert_eq!(
            merged[1].no_direct_play_audio_codecs,
            vec!["dts".to_string()]
        );
        // A second toggle on the TV builds on the saved list, not the loaded one.
        assert_eq!(
            toggled_codecs(&merged[0].no_direct_play_audio_codecs, "ac3"),
            vec!["ac3".to_string(), "eac3".to_string()]
        );
    }

    #[test]
    fn a_device_being_saved_has_its_toggles_disabled() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                AdminView {
                    users: Vec::new(),
                    current_user_id: String::new(),
                    status: None,
                    on_action: |_| {},
                    active_tab: AdminTab::Devices,
                    device_saving: std::collections::BTreeSet::from(["tv".to_string()]),
                    devices: vec![
                        DeviceEntry { id: "tv".into(), name: "TV".into(), ..Default::default() },
                        DeviceEntry { id: "pc".into(), name: "PC".into(), ..Default::default() },
                    ],
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        let row = |name: &str| {
            html.split("<tr")
                .find(|r| r.contains(&format!(">{name}</td>")))
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(row("TV").matches("disabled").count(), 4, "{html}");
        assert!(!row("PC").contains("disabled"), "{html}");
    }

    #[test]
    fn a_device_row_shows_which_sound_formats_are_converted() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                AdminView {
                    users: Vec::new(),
                    current_user_id: String::new(),
                    status: None,
                    on_action: |_| {},
                    active_tab: AdminTab::Devices,
                    devices: vec![DeviceEntry {
                        id: "tv".into(),
                        name: "Living room TV".into(),
                        no_direct_play_audio_codecs: vec!["eac3".into()],
                        ..Default::default()
                    }],
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("Sound formats converted for Living room TV"),
            "{html}"
        );
        let pressed = |label: &str| {
            html.split("<button")
                .find(|b| b.contains(&format!(">{label}</button>")))
                .map(|b| b.contains(r#"aria-pressed="true""#))
        };
        assert_eq!(pressed("Dolby Digital Plus"), Some(true), "{html}");
        assert_eq!(pressed("Dolby Digital"), Some(false), "{html}");
        assert_eq!(pressed("DTS"), Some(false), "{html}");
    }

    /// The page on `tab`, `loading` or not, as HTML.
    fn render_tab(tab: AdminTab, loading: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(tab, loading): (AdminTab, bool)| {
                rsx! {
                    AdminView {
                        users: Vec::new(),
                        current_user_id: String::new(),
                        status: None,
                        on_action: |_| {},
                        active_tab: tab,
                        loading,
                    }
                }
            },
            (tab, loading),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_loading_tab_says_so_instead_of_empty() {
        let html = render_tab(AdminTab::Devices, true);
        assert!(html.contains("Loading…"), "{html}");
        assert!(!html.contains("No devices"), "{html}");
        assert!(render_tab(AdminTab::Devices, false).contains("No devices"));
    }

    #[test]
    fn libraries_offer_the_scan_and_other_tabs_do_not() {
        assert!(render_tab(AdminTab::Libraries, false).contains("Scan all libraries"));
        assert!(!render_tab(AdminTab::Users, false).contains("Scan all libraries"));
    }

    #[test]
    fn the_open_tab_is_the_current_page_in_the_sidebar() {
        let html = render_tab(AdminTab::Logs, false);
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
        assert!(
            html.contains(r#"data-tab="logs" aria-current="page""#),
            "{html}"
        );
    }

    #[test]
    fn create_user_attempt_value_semantics() {
        let a = CreateUserAttempt {
            name: "alice".into(),
            password: "p".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn action_variants_distinct() {
        let a = AdminAction::DeleteUser("u".into());
        let b = AdminAction::Refresh;
        assert_ne!(a, b);
        // Variants with same shape but different payload still
        // compare as expected.
        assert_eq!(
            AdminAction::DeleteUser("x".into()),
            AdminAction::DeleteUser("x".into()),
        );
        assert_ne!(
            AdminAction::DeleteUser("x".into()),
            AdminAction::DeleteUser("y".into()),
        );
    }

    #[test]
    fn tabs_round_trip_label_and_suffix() {
        assert_eq!(AdminTab::Users.label(), "Users");
        assert_eq!(AdminTab::Activity.class_suffix(), "activity");
        assert_eq!(AdminTab::ScheduledTasks.label(), "Tasks");
        assert_eq!(AdminTab::Plugins.class_suffix(), "plugins");
        assert_eq!(AdminTab::Logs.label(), "Logs");
        assert_eq!(AdminTab::ApiKeys.label(), "API keys");
        assert_eq!(AdminTab::ApiKeys.class_suffix(), "apikeys");
        assert_eq!(AdminTab::Branding.label(), "Branding");
        assert_eq!(AdminTab::Branding.class_suffix(), "branding");
        assert_eq!(AdminTab::Duplicates.label(), "Duplicates");
        assert_eq!(AdminTab::Duplicates.class_suffix(), "duplicates");
        assert_eq!(AdminTab::Integrity.label(), "Damaged files");
        assert_eq!(AdminTab::Integrity.class_suffix(), "integrity");
        assert_eq!(AdminTab::ShowIdentity.label(), "Show identity");
        assert_eq!(AdminTab::ShowIdentity.class_suffix(), "showidentity");
        assert_eq!(AdminTab::all().len(), 12);
        assert_eq!(AdminTab::all()[3], AdminTab::Integrity);
        assert_eq!(AdminTab::all()[4], AdminTab::ShowIdentity);
    }

    fn version(id: &str, width: u32, height: u32) -> FilmVersion {
        FilmVersion {
            id: id.into(),
            path: format!("/m/{id}.mkv"),
            size_bytes: Some(4_500_000_000),
            width: Some(width),
            height: Some(height),
            video_codec: Some("hevc".into()),
            date_added: Some(1_762_214_400),
            edition_count: 0,
        }
    }

    #[test]
    fn the_duplicates_tab_lists_every_version_and_offers_no_delete() {
        let films = vec![DuplicateFilm {
            title: "The Six Million Dollar Man: Wine, Women and War".into(),
            production_year: Some(1973),
            provider_id: None,
            versions: vec![version("1", 1920, 1080), version("2", 720, 480)],
        }];
        let html =
            dioxus_ssr::render_element(rsx! { DuplicatesSection { films: Some(Ok(films)) } });
        assert!(html.contains("Wine, Women and War"), "{html}");
        assert!(html.contains("(1973)"), "{html}");
        assert!(html.contains("Matched on title and year"), "{html}");
        assert!(
            html.contains("/m/1.mkv") && html.contains("/m/2.mkv"),
            "{html}"
        );
        assert!(html.contains("1080p") && html.contains("720×480"), "{html}");
        assert!(html.contains("4.2 GiB"), "{html}");
        assert!(html.contains("2025-11-04"), "{html}");
        assert!(!html.to_lowercase().contains("delete"), "{html}");
    }

    #[test]
    fn the_duplicates_tab_says_when_there_are_none() {
        let html =
            dioxus_ssr::render_element(rsx! { DuplicatesSection { films: Some(Ok(vec![])) } });
        assert!(html.contains("more than once"), "{html}");
    }

    fn damaged(id: &str) -> DamagedFile {
        DamagedFile {
            media_id: id.into(),
            verdict: "damaged".into(),
            ..DamagedFile::default()
        }
    }

    fn episode() -> DamagedFile {
        DamagedFile {
            path: Some("/tv/Show/S01E02.mkv".into()),
            first_fault: Some("Invalid data found when processing input".into()),
            first_fault_ms: Some(2_533_000),
            kind: Some("episode".into()),
            name: Some("Pilot".into()),
            series_name: Some("Show".into()),
            season_number: Some(1),
            episode_number: Some(2),
            checked_at: Some(1_762_214_400),
            ..damaged("123")
        }
    }

    fn render_integrity(
        report: Option<Result<IntegrityReport, String>>,
        queued: Vec<String>,
        errors: Vec<(String, String)>,
    ) -> String {
        render_integrity_deep(report, queued, errors, vec![], vec![])
    }

    fn render_deep_search(
        search: DeepSearchState,
        deep_queued: Vec<String>,
        deep_errors: Vec<(String, String)>,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(search, deep_queued, deep_errors): (
                DeepSearchState,
                Vec<String>,
                Vec<(String, String)>,
            )| {
                rsx! {
                    IntegritySection {
                        report: deep_report(vec![], vec![]),
                        queued: vec![],
                        errors: vec![],
                        deep_queued,
                        deep_errors,
                        deep_search: search,
                        on_action: |_| {},
                    }
                }
            },
            (search, deep_queued, deep_errors),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn hit(id: &str, name: &str, media_type: &str) -> SearchHint {
        SearchHint {
            id: id.into(),
            name: name.into(),
            kind: ItemKind::from_jellyfin_type(media_type),
            media_type: media_type.into(),
            matched_term: String::new(),
        }
    }

    fn render_integrity_deep(
        report: Option<Result<IntegrityReport, String>>,
        queued: Vec<String>,
        errors: Vec<(String, String)>,
        deep_queued: Vec<String>,
        deep_errors: Vec<(String, String)>,
    ) -> String {
        type Input = (
            Option<Result<IntegrityReport, String>>,
            Vec<String>,
            Vec<(String, String)>,
            Vec<String>,
            Vec<(String, String)>,
        );
        let mut dom = VirtualDom::new_with_props(
            |(report, queued, errors, deep_queued, deep_errors): Input| {
                rsx! {
                    IntegritySection {
                        report,
                        queued,
                        errors,
                        deep_queued,
                        deep_errors,
                        deep_search: DeepSearchState::default(),
                        on_action: |_| {},
                    }
                }
            },
            (report, queued, errors, deep_queued, deep_errors),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn report(
        files: Vec<DamagedFile>,
        status: IntegrityStatus,
    ) -> Option<Result<IntegrityReport, String>> {
        Some(Ok(IntegrityReport {
            status,
            files,
            deep_checks: Ok(vec![]),
        }))
    }

    #[test]
    fn the_damaged_tab_lists_each_file_with_its_title_and_break_time() {
        let status = IntegrityStatus {
            eligible: 9_700,
            checked: 4_120,
            damaged: 2,
            queued: 0,
        };
        let other = DamagedFile {
            verdict: "short".into(),
            kind: Some("movie".into()),
            name: Some("Heat".into()),
            ..damaged("7")
        };
        let html = render_integrity(report(vec![episode(), other], status), vec![], vec![]);
        assert!(html.contains("Show · S01E02 · Pilot"), "{html}");
        assert!(html.contains("Breaks at 0:42:13"), "{html}");
        assert!(
            html.contains("Invalid data found when processing input"),
            "{html}"
        );
        assert!(html.contains("/tv/Show/S01E02.mkv"), "{html}");
        assert!(html.contains("Checked 2025-11-04"), "{html}");
        assert!(
            html.contains("Heat") && html.contains("Shorter than it claims"),
            "{html}"
        );
        assert!(html.contains("Checked 4,120 of 9,700 files"), "{html}");
        assert!(html.contains("Re-check"), "{html}");
        assert!(!html.to_lowercase().contains("delete"), "{html}");
    }

    #[test]
    fn the_damaged_tab_says_everything_is_fine_when_checking_is_complete() {
        let status = IntegrityStatus {
            eligible: 10,
            checked: 10,
            damaged: 0,
            queued: 0,
        };
        let html = render_integrity(report(vec![], status), vec![], vec![]);
        assert!(
            html.contains("Every file has been checked. Nothing is damaged."),
            "{html}"
        );
    }

    #[test]
    fn the_damaged_tab_says_checking_continues_when_incomplete() {
        let status = IntegrityStatus {
            eligible: 10,
            checked: 4,
            damaged: 0,
            queued: 0,
        };
        let html = render_integrity(report(vec![], status), vec![], vec![]);
        assert!(html.contains("Nothing damaged so far"), "{html}");
        assert!(html.contains("in the background"), "{html}");
    }

    #[test]
    fn a_pending_or_just_requested_recheck_shows_queued() {
        let status = IntegrityStatus {
            eligible: 10,
            checked: 10,
            damaged: 2,
            queued: 1,
        };
        let pending = DamagedFile {
            recheck_pending: true,
            ..damaged("1")
        };
        let html = render_integrity(report(vec![pending, damaged("2")], status), vec![], vec![]);
        assert_eq!(html.matches(">Queued<").count(), 1, "{html}");
        assert!(html.contains("1 waiting to be re-checked"), "{html}");
        let html = render_integrity(
            report(vec![damaged("1"), damaged("2")], status),
            vec!["2".into()],
            vec![],
        );
        assert_eq!(html.matches(">Queued<").count(), 1, "{html}");
    }

    #[test]
    fn a_failed_recheck_shows_its_error_on_that_row_only() {
        let status = IntegrityStatus {
            eligible: 2,
            checked: 2,
            damaged: 2,
            queued: 0,
        };
        let html = render_integrity(
            report(vec![damaged("1"), damaged("2")], status),
            vec![],
            vec![("2".into(), "status 404".into())],
        );
        assert_eq!(html.matches("status 404").count(), 1, "{html}");
    }

    #[test]
    fn a_file_gone_from_the_library_says_so() {
        let status = IntegrityStatus {
            eligible: 1,
            checked: 1,
            damaged: 1,
            queued: 0,
        };
        let html = render_integrity(report(vec![damaged("5")], status), vec![], vec![]);
        assert!(html.contains("no longer in the library"), "{html}");
    }

    fn deep(id: &str, state: &str) -> DeepCheck {
        DeepCheck {
            media_id: id.into(),
            state: state.into(),
            requested_at: 1_762_214_400,
            ..DeepCheck::default()
        }
    }

    fn deep_report(
        files: Vec<DamagedFile>,
        deep_checks: Vec<DeepCheck>,
    ) -> Option<Result<IntegrityReport, String>> {
        Some(Ok(IntegrityReport {
            status: IntegrityStatus {
                eligible: 5,
                checked: 5,
                damaged: files.len() as u64,
                queued: 0,
            },
            files,
            deep_checks: Ok(deep_checks),
        }))
    }

    #[test]
    fn each_damaged_row_offers_a_deep_check_except_books() {
        let book = DamagedFile {
            kind: Some("book".into()),
            ..damaged("2")
        };
        let html = render_integrity_deep(
            deep_report(vec![damaged("1"), book], vec![]),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(html.matches(">Deep check<").count(), 1, "{html}");
    }

    #[test]
    fn the_damaged_tab_offers_a_csv_and_shows_each_rows_spans() {
        let file = DamagedFile {
            ranges: vec![DamageRange {
                start_ms: 600_000,
                end_ms: 1_140_000,
            }],
            ..damaged("1")
        };
        let html = render_integrity_deep(
            deep_report(vec![file], vec![]),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert!(
            html.contains("download=\"pharos-damaged-files.csv\""),
            "{html}"
        );
        assert!(
            html.contains("href=\"data:text/csv;charset=utf-8,"),
            "{html}"
        );
        assert!(html.contains("Damaged at 0:10:00–0:19:00"), "{html}");

        let none =
            render_integrity_deep(deep_report(vec![], vec![]), vec![], vec![], vec![], vec![]);
        assert!(!none.contains("Export CSV"), "nothing to export: {none}");
    }

    #[test]
    fn a_row_shows_deep_check_running_while_queued_or_decoding() {
        for state in ["queued", "running"] {
            let html = render_integrity_deep(
                deep_report(vec![damaged("1")], vec![deep("1", state)]),
                vec![],
                vec![],
                vec![],
                vec![],
            );
            assert!(html.contains("Deep check running…"), "{state}: {html}");
            assert!(!html.contains(">Deep check<"), "{state}: {html}");
        }
        let html = render_integrity_deep(
            deep_report(vec![damaged("1")], vec![]),
            vec![],
            vec![],
            vec!["1".into()],
            vec![],
        );
        assert!(html.contains("Deep check running…"), "{html}");
        let done = render_integrity_deep(
            deep_report(vec![damaged("1")], vec![deep("1", "clean")]),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert!(
            done.contains(">Deep check<"),
            "a finished run can be repeated: {done}"
        );
    }

    #[test]
    fn a_deep_check_error_shows_on_its_row() {
        let html = render_integrity_deep(
            deep_report(vec![damaged("1")], vec![]),
            vec![],
            vec![],
            vec![],
            vec![("1".into(), "books have no frames to decode".into())],
        );
        assert!(
            html.contains("Could not start the deep check: books have no frames to decode"),
            "{html}"
        );
    }

    #[test]
    fn the_deep_checks_section_explains_itself_when_empty() {
        let html = render_integrity_deep(
            deep_report(vec![damaged("1")], vec![]),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert!(html.contains("Deep checks"), "{html}");
        assert!(html.contains("decodes a whole file"), "{html}");
        assert!(html.contains("takes minutes"), "{html}");
        assert!(html.contains("one file at a time"), "{html}");
    }

    #[test]
    fn a_deep_check_fault_leaves_the_rest_of_the_tab_intact() {
        let report = Ok(IntegrityReport {
            status: IntegrityStatus {
                eligible: 5,
                checked: 5,
                damaged: 1,
                queued: 0,
            },
            files: vec![damaged("1")],
            deep_checks: Err("this replica does not run deep checks".into()),
        });
        let html = render_integrity_deep(Some(report.clone()), vec![], vec![], vec![], vec![]);
        assert!(html.contains("Checked 5 of 5 files"), "{html}");
        assert!(
            html.contains("File 1"),
            "the damaged list still shows: {html}"
        );
        assert!(
            html.contains("Could not list deep checks: this replica does not run deep checks"),
            "{html}"
        );
        assert!(!html.contains("decodes a whole file"), "{html}");
        assert!(!deep_poll_wanted(&report));
    }

    #[test]
    fn the_search_box_lists_video_and_audio_hits_with_a_deep_check_button() {
        let html = render_deep_search(
            DeepSearchState {
                query: "heat".into(),
                hits: vec![
                    hit("1", "Heat", "Movie"),
                    hit("2", "Heat Wave", "Episode"),
                    hit("3", "Heat Song", "Audio"),
                ],
                ..DeepSearchState::default()
            },
            vec![],
            vec![],
        );
        assert!(html.contains("Deep check another file"), "{html}");
        for name in ["Heat", "Heat Wave", "Heat Song"] {
            assert!(html.contains(name), "{name}: {html}");
        }
        assert_eq!(html.matches(">Deep check<").count(), 3, "{html}");
    }

    #[test]
    fn the_search_box_never_offers_books_folders_or_series() {
        let html = render_deep_search(
            DeepSearchState {
                query: "heat".into(),
                hits: vec![
                    hit("1", "Heat Book", "Book"),
                    hit("2", "Heat Shows", "Series"),
                    hit("3", "Heat Folder", "Folder"),
                ],
                ..DeepSearchState::default()
            },
            vec![],
            vec![],
        );
        assert!(!html.contains("Heat Book"), "{html}");
        assert!(!html.contains("Heat Shows"), "{html}");
        assert!(!html.contains("Heat Folder"), "{html}");
        assert!(!html.contains(">Deep check<"), "{html}");
        assert!(html.contains("No video or audio files match."), "{html}");
    }

    #[test]
    fn the_search_box_offers_at_most_eight_and_shows_running_and_errors() {
        let many: Vec<SearchHint> = (0..12)
            .map(|i| hit(&i.to_string(), &format!("Film {i}"), "Movie"))
            .collect();
        let html = render_deep_search(
            DeepSearchState {
                query: "film".into(),
                hits: many,
                ..DeepSearchState::default()
            },
            vec!["0".into()],
            vec![(
                "1".into(),
                "too many deep checks are waiting; try again when some finish".into(),
            )],
        );
        assert!(html.contains("Film 7"), "{html}");
        assert!(!html.contains("Film 8"), "{html}");
        assert!(html.contains("Deep check running…"), "{html}");
        assert!(
            html.contains("Could not start the deep check: too many deep checks are waiting"),
            "{html}"
        );
    }

    #[test]
    fn the_deep_checks_section_words_each_state() {
        let checks = vec![
            deep("1", "queued"),
            deep("2", "running"),
            deep("3", "clean"),
            DeepCheck {
                errors: 4,
                first_error: Some("Invalid NAL unit size".into()),
                first_error_ms: Some(2_533_000),
                elapsed_secs: Some(252),
                finished_at: Some(1_762_214_400),
                name: Some("Heat".into()),
                kind: Some("movie".into()),
                ..deep("4", "damaged")
            },
            DeepCheck {
                detail: Some("ffmpeg is not installed".into()),
                ..deep("5", "failed")
            },
        ];
        let html =
            render_integrity_deep(deep_report(vec![], checks), vec![], vec![], vec![], vec![]);
        for want in [
            "Waiting",
            "Decoding…",
            "No damage found",
            "Damaged: 4 decode errors",
            "Breaks at 0:42:13",
            "Invalid NAL unit size",
            "Took 4m 12s",
            "Finished 2025-11-04",
            "Couldn&#39;t run: ffmpeg is not installed",
            "Heat",
            "File 1",
        ] {
            assert!(html.contains(want), "missing {want:?}: {html}");
        }
        assert!(!html.contains("decodes a whole file"), "{html}");
    }

    #[test]
    fn deep_state_wording_handles_edges() {
        assert_eq!(
            deep_state_words(&DeepCheck {
                errors: 1,
                ..deep("1", "damaged")
            }),
            "Damaged: 1 decode error"
        );
        assert_eq!(deep_state_words(&deep("1", "failed")), "Couldn't run");
        assert_eq!(
            deep_state_words(&deep("1", "weird")),
            "Unknown state: weird"
        );
        assert_eq!(duration_label(7), "7s");
        assert_eq!(duration_label(3_725), "1h 02m");
        assert_eq!(deep_timing(&deep("1", "queued")), None);
        assert!(any_deep_active(&[deep("1", "clean"), deep("2", "running")]));
        assert!(!any_deep_active(&[deep("1", "clean"), deep("2", "failed")]));
        let with = |s: &str| {
            Ok(IntegrityReport {
                deep_checks: Ok(vec![deep("1", s)]),
                ..IntegrityReport::default()
            })
        };
        assert!(deep_poll_wanted(&with("running")));
        assert!(deep_poll_wanted(&with("queued")));
        assert!(!deep_poll_wanted(&with("clean")));
        assert!(!deep_poll_wanted(&Err("boom".into())));
        assert!(generation_current(3, 3));
        assert!(
            !generation_current(2, 3),
            "a superseded timer must not re-load"
        );
        assert!(deep_in_progress(&[deep("1", "queued")], "1"));
        assert!(!deep_in_progress(&[deep("1", "queued")], "2"));
    }

    #[test]
    fn the_damaged_tab_shows_loading_and_errors() {
        let html = render_integrity(None, vec![], vec![]);
        assert!(html.contains("pharos-loading"), "{html}");
        let html = render_integrity(Some(Err("it broke".into())), vec![], vec![]);
        assert!(html.contains("it broke"), "{html}");
    }

    #[test]
    fn titles_fall_back_from_name_to_file_name() {
        assert_eq!(damaged_title(&episode()), "Show · S01E02 · Pilot");
        let no_code = DamagedFile {
            season_number: None,
            ..episode()
        };
        assert_eq!(damaged_title(&no_code), "Show · Pilot");
        let movie = DamagedFile {
            kind: Some("movie".into()),
            name: Some("Heat".into()),
            ..damaged("1")
        };
        assert_eq!(damaged_title(&movie), "Heat");
        let bare = DamagedFile {
            path: Some("/m/dir/Heat (1995).mkv".into()),
            ..damaged("1")
        };
        assert_eq!(damaged_title(&bare), "Heat (1995).mkv");
        let bare_episode = DamagedFile {
            kind: Some("episode".into()),
            ..bare
        };
        assert_eq!(damaged_title(&bare_episode), "Heat (1995).mkv");
        assert_eq!(damaged_title(&damaged("9")), "File 9");
    }

    #[test]
    fn times_verdicts_and_counts_read_plainly() {
        assert_eq!(hms(2_533_000), "0:42:13");
        assert_eq!(hms(3_725_999), "1:02:05");
        assert_eq!(hms(0), "0:00:00");
        assert_eq!(break_label(61_000), "Breaks at 0:01:01");
        assert_eq!(verdict_words("damaged"), "Damaged");
        assert_eq!(verdict_words("short"), "Shorter than it claims");
        assert_eq!(verdict_words("unreadable"), "Can't be read");
        assert_eq!(verdict_words("surprise"), "Damaged");
        assert_eq!(kind_label(Some("audio")), "Audio");
        assert_eq!(kind_label(None), "File");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn the_summary_names_damage_and_waiting_files() {
        let s = IntegrityStatus {
            eligible: 9_700,
            checked: 4_120,
            damaged: 3,
            queued: 2,
        };
        assert_eq!(
            integrity_summary(&s),
            "Checked 4,120 of 9,700 files · 3 damaged · 2 waiting to be re-checked. Checking continues in the background."
        );
        let done = IntegrityStatus {
            eligible: 5,
            checked: 5,
            damaged: 1,
            queued: 0,
        };
        assert_eq!(
            integrity_summary(&done),
            "Checked 5 of 5 files · 1 damaged."
        );
    }

    #[test]
    fn sizes_scale_to_the_file() {
        let sized = |b| FilmVersion {
            size_bytes: Some(b),
            ..FilmVersion::default()
        };
        assert_eq!(size_label(&sized(700 * 1024)), "700 KiB");
        assert_eq!(size_label(&sized(700 * 1024 * 1024)), "700 MiB");
        assert_eq!(size_label(&FilmVersion::default()), "—");
    }

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    /// The Show identity page as HTML.
    fn render_identity(
        identity: Option<Result<SeriesIdentity, String>>,
        search: IdentitySearchState,
        prompt: Option<PickPrompt>,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(identity, search, prompt): (
                Option<Result<SeriesIdentity, String>>,
                IdentitySearchState,
                Option<PickPrompt>,
            )| {
                rsx! {
                    ShowIdentitySection { identity, search, prompt, on_action: |_| {} }
                }
            },
            (identity, search, prompt),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn identity_page() -> SeriesIdentity {
        let key = |year: u32| format!("/tv/Cosmos{KEY_YEAR_SEP}{year}");
        SeriesIdentity {
            repair_mode: "dry_run".into(),
            splits: vec![
                SplitFolder {
                    folder: "/tv/Cosmos".into(),
                    status: "pending".into(),
                    groups: vec![
                        SplitGroup {
                            series_key: key(1980),
                            year: Some(1980),
                            paths: vec!["/tv/Cosmos/a.mkv".into()],
                        },
                        SplitGroup {
                            series_key: key(2014),
                            year: Some(2014),
                            paths: vec!["/tv/Cosmos/b.mkv".into(), "/tv/Cosmos/c.mkv".into()],
                        },
                    ],
                    moves: vec![crate::client::PendingMove {
                        path: "/tv/Cosmos/b.mkv".into(),
                        from_key: "/tv/Cosmos".into(),
                        from_year: None,
                        to_key: key(2014),
                        to_year: Some(2014),
                    }],
                },
                SplitFolder {
                    folder: "/tv/Fate".into(),
                    status: "applied".into(),
                    ..SplitFolder::default()
                },
            ],
            flagged: vec![FlaggedMatch {
                series_key: "Fate".into(),
                series_id: "000000000000002a000000000000002a".into(),
                name: "Fate".into(),
                provider: Some("tvdb".into()),
                external_id: Some("81".into()),
                premiere_date: Some(1_100_000_000),
                runner_up_id: Some("82".into()),
                runner_up_score: Some(0.8),
            }],
            overrides: vec![HandFix {
                id: 7,
                folder: "/tv/Cosmos".into(),
                keeps_together: true,
                source: "admin".into(),
                created_by: Some("ali".into()),
                created_at: 1_700_000_000,
                ..HandFix::default()
            }],
        }
    }

    #[test]
    fn a_split_key_reads_as_folder_name_and_year_without_the_separator() {
        let key = format!("/tv/Cosmos{KEY_YEAR_SEP}2014");
        assert_eq!(key_label(&key, None), "Cosmos (2014)");
        assert_eq!(key_label("/tv/Cosmos", Some(1980)), "Cosmos (1980)");
        assert_eq!(key_label("Cosmos", None), "Cosmos");
        assert_eq!(folder_name("/tv/Cosmos/"), "Cosmos");
        assert_eq!(folder_name("/"), "/");
    }

    #[test]
    fn a_pending_split_says_when_it_lands() {
        let pending = SplitFolder {
            status: "pending".into(),
            ..SplitFolder::default()
        };
        assert_eq!(split_status_label(&pending, "dry_run"), "pending (dry run)");
        assert_eq!(split_status_label(&pending, "apply"), "pending (next scan)");
        let applied = SplitFolder {
            status: "applied".into(),
            ..SplitFolder::default()
        };
        assert_eq!(split_status_label(&applied, "apply"), "applied");
    }

    #[test]
    fn the_page_lists_splits_flagged_matches_and_hand_fixes_in_words() {
        let html = render_identity(
            Some(Ok(identity_page())),
            IdentitySearchState::default(),
            None,
        );
        assert!(
            html.contains("Cosmos (1980)") && html.contains("Cosmos (2014)"),
            "{html}"
        );
        assert!(html.contains("2 files"), "{html}");
        assert!(
            html.contains("pending (dry run)") && html.contains("applied"),
            "{html}"
        );
        assert!(html.contains("title=\"/tv/Cosmos\""), "{html}");
        assert!(html.contains("1 file still to move"), "{html}");
        assert!(html.contains("href=\"/ui/browse/s42\""), "{html}");
        assert!(html.contains("scored 0.80"), "{html}");
        assert!(html.contains("Kept together as one show."), "{html}");
        assert!(html.contains("Set by ali on 2023-11-14"), "{html}");
        // The key rides in an attribute value; no text node shows it.
        let shown = html
            .split('<')
            .filter_map(|t| t.split_once('>'))
            .map(|(_, text)| text);
        assert!(shown.into_iter().all(|t| !t.contains('\u{1f}')), "{html}");
    }

    #[test]
    fn a_fix_says_what_it_moved_and_what_it_rebuilds() {
        let done = |moved: usize, rebuilding| FixDone {
            moved: (0..moved).map(|i| format!("/p{i}")).collect(),
            rebuilding,
        };
        assert_eq!(
            fix_summary(&done(3, 12)),
            "Moved 3 files; rebuilding 12 episodes in the background."
        );
        assert_eq!(
            fix_summary(&done(1, 1)),
            "Moved 1 file; rebuilding 1 episode in the background."
        );
        assert_eq!(
            fix_summary(&done(0, 4)),
            "Rebuilding 4 episodes in the background."
        );
        assert_eq!(fix_summary(&done(2, 0)), "Moved 2 files.");
        assert_eq!(fix_summary(&done(0, 0)), "Done; nothing needed rebuilding.");
    }

    #[test]
    fn a_move_to_a_new_group_prompts_for_its_show_until_one_is_picked() {
        let render = |prompt: Option<PickPrompt>| {
            render_identity(
                Some(Ok(identity_page())),
                IdentitySearchState::default(),
                prompt,
            )
        };
        let prompted = render(Some(PickPrompt {
            folder: "/tv/Cosmos".into(),
            year: Some(2014),
        }));
        assert!(prompted.contains(PICK_PROMPT), "{prompted}");
        // The group's pick form is already open, with its name filled in.
        assert!(prompted.contains("Year (optional)"), "{prompted}");
        let quiet = render(None);
        assert!(!quiet.contains(PICK_PROMPT), "{quiet}");
        assert!(!quiet.contains("Year (optional)"), "{quiet}");
        // A prompt for another folder leaves this one alone.
        let other = render(Some(PickPrompt {
            folder: "/tv/Else".into(),
            year: Some(2014),
        }));
        assert!(!other.contains(PICK_PROMPT), "{other}");
    }

    #[test]
    fn each_entry_offers_its_actions() {
        let html = render_identity(
            Some(Ok(identity_page())),
            IdentitySearchState::default(),
            None,
        );
        assert!(html.contains("Keep together"), "{html}");
        assert!(html.contains("Confirm this match"), "{html}");
        assert!(html.contains("Pick another show"), "{html}");
        assert!(html.contains("Pick the show"), "{html}");
        assert!(html.contains("Send ticked files to"), "{html}");
        assert!(html.contains("A new group"), "{html}");
        assert!(html.contains("pharos-admin-identity-remove"), "{html}");
        // One group alone has nothing to keep together.
        let single = SeriesIdentity {
            splits: vec![SplitFolder {
                folder: "/tv/One".into(),
                status: "applied".into(),
                groups: vec![SplitGroup::default()],
                ..SplitFolder::default()
            }],
            ..SeriesIdentity::default()
        };
        let html = render_identity(Some(Ok(single)), IdentitySearchState::default(), None);
        assert!(!html.contains("Keep together"), "{html}");
    }

    #[test]
    fn the_pick_form_lists_only_its_own_search() {
        let hit = ShowCandidate {
            provider: "tvdb".into(),
            id: "81".into(),
            name: "Cosmos".into(),
            year: Some(1980),
        };
        let render = |series_key: &str| {
            let search = IdentitySearchState {
                series_key: series_key.to_string(),
                hits: vec![hit.clone()],
                ..IdentitySearchState::default()
            };
            let mut dom = VirtualDom::new_with_props(
                |search: IdentitySearchState| {
                    rsx! {
                        PickForm {
                            series_key: "/tv/Cosmos\u{1f}1980".to_string(),
                            default_name: "Cosmos".to_string(),
                            default_year: Some(1980),
                            search,
                            on_action: |_| {},
                        }
                    }
                },
                search,
            );
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        let mine = render("/tv/Cosmos\u{1f}1980");
        assert!(mine.contains("Cosmos (1980) · tvdb 81"), "{mine}");
        assert!(mine.contains("This is the show"), "{mine}");
        assert!(!render("/tv/Other").contains("This is the show"));
    }

    #[test]
    fn an_empty_identity_says_nothing_needs_a_look() {
        let html = render_identity(
            Some(Ok(SeriesIdentity::default())),
            IdentitySearchState::default(),
            None,
        );
        assert!(html.contains("Nothing needs a look."), "{html}");
        let failed = render_identity(
            Some(Err("status 500".to_string())),
            IdentitySearchState::default(),
            None,
        );
        assert!(failed.contains("status 500"), "{failed}");
    }
}
