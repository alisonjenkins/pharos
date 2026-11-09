//! Dioxus admin UI mirroring `api::jellyfin::admin` endpoints (T50, T58).
//!
//! Phase 1 (T50): user list with delete + create form + library refresh.
//! Phase 2 (T58): tabbed dashboard — Users, Libraries (VirtualFolders),
//! Devices, Activity log. Tabs that read a server-side empty stub still
//! render so the UI is in place when entries land.

use crate::client::{
    ActivityEntry, AdminUser, ApiKey, BrandingConfig, DamagedFile, DeviceEntry, DuplicateFilm,
    FilmVersion, IntegrityReport, IntegrityStatus, LibraryFolder, LogEntry, PluginEntry,
    ScheduledTask,
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
    LibraryRefresh,
    Refresh,
    SelectTab(AdminTab),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdminTab {
    #[default]
    Users,
    Libraries,
    Duplicates,
    Integrity,
    Devices,
    Activity,
    ScheduledTasks,
    Plugins,
    Logs,
    ApiKeys,
    Branding,
}

impl AdminTab {
    fn label(self) -> &'static str {
        match self {
            Self::Users => "Users",
            Self::Libraries => "Libraries",
            Self::Duplicates => "Duplicates",
            Self::Integrity => "Damaged files",
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
            Self::Devices => "devices",
            Self::Activity => "activity",
            Self::ScheduledTasks => "scheduledtasks",
            Self::Plugins => "plugins",
            Self::Logs => "logs",
            Self::ApiKeys => "apikeys",
            Self::Branding => "branding",
        }
    }
    fn all() -> [AdminTab; 11] {
        [
            Self::Users,
            Self::Libraries,
            Self::Duplicates,
            Self::Integrity,
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
    /// Files whose Re-check was just requested from this page.
    #[props(default)]
    pub integrity_queued: Vec<String>,
    /// Re-check failures by media id, shown on the row.
    #[props(default)]
    pub integrity_errors: Vec<(String, String)>,
}

#[component]
pub fn AdminView(props: AdminViewProps) -> Element {
    let new_name = use_signal(String::new);
    let new_password = use_signal(String::new);
    let mut new_name_sig = new_name;
    let mut new_password_sig = new_password;
    let active_tab = props.active_tab;

    rsx! {
        div {
            class: "pharos-admin",
            "data-tab": "{active_tab.class_suffix()}",
            header { class: "pharos-admin-header", h2 { "Admin" } }

            nav {
                class: "pharos-admin-tabs",
                for t in AdminTab::all() {
                    button {
                        key: "{t.class_suffix()}",
                        class: if t == active_tab {
                            "pharos-admin-tab on"
                        } else {
                            "pharos-admin-tab off"
                        },
                        onclick: {
                            let on_action = props.on_action;
                            move |_| on_action.call(AdminAction::SelectTab(t))
                        },
                        "{t.label()}"
                    }
                }
            }

            if let Some(msg) = props.status.as_ref() {
                p { class: "pharos-admin-status", "{msg}" }
            }

            match active_tab {
                AdminTab::Users => rsx! {
                    section {
                        class: "pharos-admin-section pharos-admin-section-users",
                        div {
                            class: "pharos-admin-actions",
                            button {
                                class: "pharos-admin-refresh-library",
                                onclick: {
                                    let on_action = props.on_action;
                                    move |_| on_action.call(AdminAction::LibraryRefresh)
                                },
                                "Library Refresh"
                            }
                            button {
                                class: "pharos-admin-reload",
                                onclick: {
                                    let on_action = props.on_action;
                                    move |_| on_action.call(AdminAction::Refresh)
                                },
                                "Reload"
                            }
                        }

                        h3 { "Users" }
                        ul {
                            class: "pharos-admin-user-list",
                            for user in props.users.iter().cloned() {
                                AdminUserRow {
                                    key: "{user.id}",
                                    disable_delete: user.id == props.current_user_id,
                                    is_self: user.id == props.current_user_id,
                                    user: user.clone(),
                                    on_action: props.on_action,
                                }
                            }
                        }

                        h3 { "New user" }
                        form {
                            class: "pharos-admin-create",
                            onsubmit: {
                                let on_action = props.on_action;
                                move |ev: FormEvent| {
                                    ev.prevent_default();
                                    let name = new_name_sig.read().clone();
                                    let password = new_password_sig.read().clone();
                                    if name.is_empty() {
                                        return;
                                    }
                                    on_action.call(AdminAction::CreateUser(CreateUserAttempt {
                                        name,
                                        password,
                                    }));
                                    new_name_sig.set(String::new());
                                    new_password_sig.set(String::new());
                                }
                            },
                            input {
                                class: "pharos-admin-new-name",
                                r#type: "text",
                                placeholder: "Username",
                                value: "{new_name.read()}",
                                oninput: move |ev| new_name_sig.set(ev.value()),
                            }
                            input {
                                class: "pharos-admin-new-password",
                                r#type: "password",
                                placeholder: "Password",
                                value: "{new_password.read()}",
                                oninput: move |ev| new_password_sig.set(ev.value()),
                            }
                            button { r#type: "submit", "Create" }
                        }
                    }
                },
                AdminTab::Libraries => rsx! {
                    section {
                        class: "pharos-admin-section pharos-admin-section-libraries",
                        h3 { "Libraries" }
                        if props.libraries.is_empty() {
                            p { class: "pharos-empty", "No libraries configured" }
                        } else {
                            ul {
                                class: "pharos-admin-library-list",
                                for lib in props.libraries.iter() {
                                    li {
                                        class: "pharos-admin-library",
                                        key: "{lib.item_id}",
                                        span { class: "pharos-admin-library-name", "{lib.name}" }
                                        span {
                                            class: "pharos-admin-library-kind",
                                            "{lib.collection_type}"
                                        }
                                        ul {
                                            class: "pharos-admin-library-locations",
                                            for loc in lib.locations.iter() {
                                                li {
                                                    key: "{loc}",
                                                    "{loc}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                AdminTab::Devices => rsx! {
                    section {
                        class: "pharos-admin-section pharos-admin-section-devices",
                        h3 { "Devices" }
                        if props.devices.is_empty() {
                            p { class: "pharos-empty", "No devices have connected" }
                        } else {
                            table {
                                class: "pharos-admin-device-table",
                                thead {
                                    tr {
                                        th { "Device" }
                                        th { "App" }
                                        th { "Last user" }
                                    }
                                }
                                tbody {
                                    for d in props.devices.iter() {
                                        tr {
                                            key: "{d.id}",
                                            td { "{d.name}" }
                                            td { "{d.app_name}" }
                                            td { "{d.last_user_name}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                AdminTab::Activity => rsx! {
                    section {
                        class: "pharos-admin-section pharos-admin-section-activity",
                        h3 { "Activity log" }
                        if props.activity.is_empty() {
                            p { class: "pharos-empty", "No activity recorded" }
                        } else {
                            ul {
                                class: "pharos-admin-activity-list",
                                for e in props.activity.iter() {
                                    li {
                                        class: "pharos-admin-activity",
                                        key: "{e.id}",
                                        span { class: "pharos-admin-activity-date", "{e.date_iso}" }
                                        span { class: "pharos-admin-activity-severity", "{e.severity}" }
                                        span { class: "pharos-admin-activity-name", "{e.name}" }
                                        if !e.short_overview.is_empty() {
                                            span {
                                                class: "pharos-admin-activity-overview",
                                                "{e.short_overview}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                AdminTab::ScheduledTasks => rsx! {
                    section {
                        class: "pharos-admin-section pharos-admin-section-scheduledtasks",
                        h3 { "Scheduled tasks" }
                        if props.scheduled_tasks.is_empty() {
                            p { class: "pharos-empty", "No scheduled tasks registered" }
                        } else {
                            table {
                                class: "pharos-admin-tasks-table",
                                thead {
                                    tr {
                                        th { "Task" }
                                        th { "Category" }
                                        th { "State" }
                                        th { "Last run" }
                                    }
                                }
                                tbody {
                                    for t in props.scheduled_tasks.iter() {
                                        tr {
                                            key: "{t.id}",
                                            td { "{t.name}" }
                                            td { "{t.category}" }
                                            td { class: "pharos-admin-task-state", "{t.state}" }
                                            td { "{t.last_execution_iso}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                AdminTab::Duplicates => rsx! {
                    DuplicatesSection { films: props.duplicates.clone() }
                },
                AdminTab::Integrity => rsx! {
                    IntegritySection {
                        report: props.integrity.clone(),
                        queued: props.integrity_queued.clone(),
                        errors: props.integrity_errors.clone(),
                        on_action: props.on_action,
                    }
                },
                AdminTab::Plugins => rsx! {
                    section {
                        class: "pharos-admin-section pharos-admin-section-plugins",
                        h3 { "Plugins" }
                        if props.plugins.is_empty() {
                            p { class: "pharos-empty", "No plugins installed" }
                        } else {
                            ul {
                                class: "pharos-admin-plugin-list",
                                for p in props.plugins.iter() {
                                    li {
                                        class: "pharos-admin-plugin",
                                        key: "{p.id}",
                                        span { class: "pharos-admin-plugin-name", "{p.name}" }
                                        span { class: "pharos-admin-plugin-version", " · v{p.version}" }
                                        if !p.status.is_empty() {
                                            span { class: "pharos-admin-plugin-status", " ({p.status})" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                AdminTab::Logs => rsx! {
                    section {
                        class: "pharos-admin-section pharos-admin-section-logs",
                        h3 { "Log files" }
                        if props.logs.is_empty() {
                            p { class: "pharos-empty", "No log files" }
                        } else {
                            ul {
                                class: "pharos-admin-log-list",
                                for l in props.logs.iter() {
                                    li {
                                        class: "pharos-admin-log",
                                        key: "{l.name}",
                                        span { class: "pharos-admin-log-name", "{l.name}" }
                                        span { class: "pharos-admin-log-size", " · {l.size_bytes} B" }
                                        if !l.date_modified_iso.is_empty() {
                                            span {
                                                class: "pharos-admin-log-date",
                                                " · {l.date_modified_iso}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                AdminTab::ApiKeys => rsx! {
                    ApiKeysPane {
                        keys: props.api_keys.clone(),
                        new_secret: props.new_api_key_secret.clone(),
                        on_action: props.on_action,
                    }
                },
                AdminTab::Branding => rsx! {
                    BrandingPane {
                        branding: props.branding.clone(),
                        on_action: props.on_action,
                    }
                },
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
    let mut name_for_submit = new_name;
    rsx! {
        section {
            class: "pharos-admin-section pharos-admin-section-apikeys",
            h3 { "API keys" }
            if let Some(secret) = new_secret.as_ref() {
                p {
                    class: "pharos-admin-apikey-new",
                    "New key (copy now — won't be shown again): "
                    code { class: "pharos-admin-apikey-value", "{secret}" }
                }
            }
            form {
                class: "pharos-admin-apikey-create",
                onsubmit: {
                    let on_action = on_action;
                    move |ev: FormEvent| {
                        ev.prevent_default();
                        let name = name_for_submit.read().clone();
                        let trimmed = name.trim().to_string();
                        if trimmed.is_empty() {
                            return;
                        }
                        on_action.call(AdminAction::CreateApiKey { app_name: trimmed });
                        name_for_submit.set(String::new());
                    }
                },
                input {
                    class: "pharos-admin-apikey-name",
                    r#type: "text",
                    placeholder: "App name",
                    value: "{new_name.read()}",
                    oninput: move |ev| new_name.set(ev.value()),
                }
                button {
                    class: "pharos-admin-apikey-create-submit",
                    r#type: "submit",
                    "Issue key"
                }
            }
            if keys.is_empty() {
                p { class: "pharos-empty", "No API keys issued" }
            } else {
                ul {
                    class: "pharos-admin-apikey-list",
                    for k in keys.iter().cloned() {
                        ApiKeyRow {
                            key: "{k.id}",
                            entry: k,
                            on_action: on_action,
                        }
                    }
                }
            }
        }
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
    rsx! {
        section {
            class: "pharos-admin-section pharos-admin-section-branding",
            h3 { "Branding" }
            form {
                class: "pharos-admin-branding-form",
                onsubmit: move |ev: FormEvent| {
                    ev.prevent_default();
                    on_action.call(AdminAction::SaveBranding(draft.read().clone()));
                },
                label {
                    class: "pharos-admin-branding-row",
                    "Server name: "
                    input {
                        class: "pharos-admin-branding-name",
                        r#type: "text",
                        value: "{d.server_name}",
                        oninput: move |ev| {
                            let mut c = draft.read().clone();
                            c.server_name = ev.value();
                            draft.set(c);
                        },
                    }
                }
                label {
                    class: "pharos-admin-branding-row",
                    "Login disclaimer: "
                    textarea {
                        class: "pharos-admin-branding-disclaimer",
                        value: "{d.login_disclaimer}",
                        oninput: move |ev| {
                            let mut c = draft.read().clone();
                            c.login_disclaimer = ev.value();
                            draft.set(c);
                        },
                    }
                }
                label {
                    class: "pharos-admin-branding-row",
                    "Custom CSS: "
                    textarea {
                        class: "pharos-admin-branding-css",
                        value: "{d.custom_css}",
                        oninput: move |ev| {
                            let mut c = draft.read().clone();
                            c.custom_css = ev.value();
                            draft.set(c);
                        },
                    }
                }
                button {
                    class: "pharos-admin-branding-save",
                    r#type: "submit",
                    "Save"
                }
            }
        }
    }
}

#[component]
fn ApiKeyRow(entry: ApiKey, on_action: EventHandler<AdminAction>) -> Element {
    let id_for_revoke = entry.id.clone();
    rsx! {
        li {
            class: "pharos-admin-apikey",
            "data-apikey-id": "{entry.id}",
            span { class: "pharos-admin-apikey-app", "{entry.app_name}" }
            if !entry.date_created_iso.is_empty() {
                span {
                    class: "pharos-admin-apikey-date",
                    " · issued {entry.date_created_iso}"
                }
            }
            button {
                class: "pharos-admin-apikey-revoke",
                onclick: move |_| on_action.call(AdminAction::RevokeApiKey {
                    key_id: id_for_revoke.clone(),
                }),
                "Revoke"
            }
        }
    }
}

#[derive(PartialEq, Clone, Props)]
struct AdminUserRowProps {
    user: AdminUser,
    disable_delete: bool,
    /// True when the row represents the currently signed-in admin —
    /// disables `Delete` + flips the password reset form into a
    /// self-change shape (needs current pw — phase 3).
    is_self: bool,
    on_action: EventHandler<AdminAction>,
}

#[component]
fn AdminUserRow(props: AdminUserRowProps) -> Element {
    let admin_marker = if props.user.is_admin { " (admin)" } else { "" };
    let id_for_delete = props.user.id.clone();
    let id_for_policy = props.user.id.clone();
    let id_for_reset = props.user.id.clone();
    let user_is_admin = props.user.is_admin;
    let mut reset_pw = use_signal(String::new);
    let mut reset_pw_for_submit = reset_pw;
    rsx! {
        li {
            class: "pharos-admin-user",
            span { class: "pharos-admin-user-name", "{props.user.name}{admin_marker}" }
            label {
                class: "pharos-admin-user-policy",
                input {
                    r#type: "checkbox",
                    checked: user_is_admin,
                    onchange: {
                        let on_action = props.on_action;
                        move |ev: FormEvent| {
                            let new_admin = ev.value() == "true" || ev.value() == "on";
                            on_action.call(AdminAction::SetUserPolicy {
                                user_id: id_for_policy.clone(),
                                is_admin: new_admin,
                            });
                        }
                    },
                }
                " admin"
            }
            form {
                class: "pharos-admin-user-reset",
                onsubmit: {
                    let on_action = props.on_action;
                    move |ev: FormEvent| {
                        ev.prevent_default();
                        let pw = reset_pw_for_submit.read().clone();
                        if pw.is_empty() {
                            return;
                        }
                        on_action.call(AdminAction::ResetUserPassword {
                            user_id: id_for_reset.clone(),
                            new_password: pw,
                        });
                        reset_pw_for_submit.set(String::new());
                    }
                },
                input {
                    class: "pharos-admin-user-reset-input",
                    r#type: "password",
                    placeholder: if props.is_self { "Reset (current pw needed)" } else { "Reset password" },
                    value: "{reset_pw.read()}",
                    oninput: move |ev| reset_pw.set(ev.value()),
                    // Self-reset still requires current pw — the bare
                    // admin reset form skips it. Disable submit on the
                    // self row to point users at the prefs view.
                    disabled: props.is_self,
                }
                button {
                    class: "pharos-admin-user-reset-submit",
                    r#type: "submit",
                    disabled: props.is_self,
                    "Reset"
                }
            }
            button {
                class: "pharos-admin-user-delete",
                disabled: props.disable_delete,
                onclick: {
                    let on_action = props.on_action;
                    move |_| on_action.call(AdminAction::DeleteUser(id_for_delete.clone()))
                },
                if props.disable_delete { "you" } else { "Delete" }
            }
        }
    }
}

/// Read-only on purpose: the admin compares versions here and removes
/// files outside pharos (V285).
#[component]
fn DuplicatesSection(films: Option<Result<Vec<DuplicateFilm>, String>>) -> Element {
    rsx! {
        section {
            class: "pharos-admin-section pharos-admin-section-duplicates",
            h3 { "Duplicate films" }
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

/// No delete on purpose: the owner decides what to do with a damaged file
/// outside pharos, then asks for it to be checked again.
#[component]
fn IntegritySection(
    report: Option<Result<IntegrityReport, String>>,
    queued: Vec<String>,
    errors: Vec<(String, String)>,
    on_action: EventHandler<AdminAction>,
) -> Element {
    rsx! {
        section {
            class: "pharos-admin-section pharos-admin-section-integrity",
            h3 { "Damaged files" }
            match report {
                None => rsx! { p { class: "pharos-loading", "Checking on the files…" } },
                Some(Err(e)) => rsx! { p { class: "pharos-error", "Could not list damaged files: {e}" } },
                Some(Ok(report)) => rsx! {
                    p { class: "pharos-admin-integrity-summary", "{integrity_summary(&report.status)}" }
                    for f in report.files.iter() {
                        DamagedRow {
                            key: "{f.media_id}",
                            file: f.clone(),
                            queued: f.recheck_pending || queued.contains(&f.media_id),
                            error: errors.iter().find(|(id, _)| *id == f.media_id).map(|(_, e)| e.clone()),
                            on_action: on_action,
                        }
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
    on_action: EventHandler<AdminAction>,
) -> Element {
    let id = file.media_id.clone();
    let breaks = file.first_fault_ms.map(break_label);
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
            if let Some(e) = error {
                p { class: "pharos-error", "Could not queue the re-check: {e}" }
            }
        }
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
    let named = f.name.as_deref().filter(|n| !n.is_empty());
    let title = if f.kind.as_deref() == Some("episode") {
        let code = match (f.season_number, f.episode_number) {
            (Some(s), Some(e)) => Some(format!("S{s:02}E{e:02}")),
            _ => None,
        };
        let parts: Vec<String> = [
            f.series_name.clone().filter(|n| !n.is_empty()),
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
        .or_else(|| f.path.as_deref().map(|p| file_name_of(p).to_string()))
        .unwrap_or_else(|| format!("File {}", f.media_id))
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

    #[test]
    fn admin_view_module_exports_present() {
        fn _f(p: AdminViewProps) -> Element {
            AdminView(p)
        }
        let _ = _f;
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
        assert_eq!(AdminTab::all().len(), 11);
        assert_eq!(AdminTab::all()[3], AdminTab::Integrity);
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
        type Input = (
            Option<Result<IntegrityReport, String>>,
            Vec<String>,
            Vec<(String, String)>,
        );
        let mut dom = VirtualDom::new_with_props(
            |(report, queued, errors): Input| {
                rsx! {
                    IntegritySection { report, queued, errors, on_action: |_| {} }
                }
            },
            (report, queued, errors),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn report(
        files: Vec<DamagedFile>,
        status: IntegrityStatus,
    ) -> Option<Result<IntegrityReport, String>> {
        Some(Ok(IntegrityReport { status, files }))
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
}
