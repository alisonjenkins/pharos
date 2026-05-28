//! Dioxus admin UI mirroring `api::jellyfin::admin` endpoints (T50, T58).
//!
//! Phase 1 (T50): user list with delete + create form + library refresh.
//! Phase 2 (T58): tabbed dashboard — Users, Libraries (VirtualFolders),
//! Devices, Activity log. Tabs that read a server-side empty stub still
//! render so the UI is in place when entries land.

use crate::client::{
    ActivityEntry, AdminUser, ApiKey, BrandingConfig, DeviceEntry, LibraryFolder, LogEntry,
    PluginEntry, ScheduledTask,
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
    LibraryRefresh,
    Refresh,
    SelectTab(AdminTab),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdminTab {
    #[default]
    Users,
    Libraries,
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
            Self::Devices => "devices",
            Self::Activity => "activity",
            Self::ScheduledTasks => "scheduledtasks",
            Self::Plugins => "plugins",
            Self::Logs => "logs",
            Self::ApiKeys => "apikeys",
            Self::Branding => "branding",
        }
    }
    fn all() -> [AdminTab; 9] {
        [
            Self::Users,
            Self::Libraries,
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
        assert_eq!(AdminTab::all().len(), 9);
    }
}
