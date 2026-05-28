//! Dioxus admin UI mirroring `api::jellyfin::admin` endpoints (T50, T58).
//!
//! Phase 1 (T50): user list with delete + create form + library refresh.
//! Phase 2 (T58): tabbed dashboard — Users, Libraries (VirtualFolders),
//! Devices, Activity log. Tabs that read a server-side empty stub still
//! render so the UI is in place when entries land.

use crate::client::{ActivityEntry, AdminUser, DeviceEntry, LibraryFolder};
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
}

impl AdminTab {
    fn label(self) -> &'static str {
        match self {
            Self::Users => "Users",
            Self::Libraries => "Libraries",
            Self::Devices => "Devices",
            Self::Activity => "Activity",
        }
    }
    fn class_suffix(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Libraries => "libraries",
            Self::Devices => "devices",
            Self::Activity => "activity",
        }
    }
    fn all() -> [AdminTab; 4] {
        [Self::Users, Self::Libraries, Self::Devices, Self::Activity]
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
                                    disable_delete: user.id == props.current_user_id,
                                    user: user.clone(),
                                    on_delete: {
                                        let on_action = props.on_action;
                                        move |id: String| on_action.call(AdminAction::DeleteUser(id))
                                    }
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
            }
        }
    }
}

#[derive(PartialEq, Clone, Props)]
struct AdminUserRowProps {
    user: AdminUser,
    disable_delete: bool,
    on_delete: EventHandler<String>,
}

#[component]
fn AdminUserRow(props: AdminUserRowProps) -> Element {
    let admin_marker = if props.user.is_admin { " (admin)" } else { "" };
    let id_for_handler = props.user.id.clone();
    rsx! {
        li {
            class: "pharos-admin-user",
            span { class: "pharos-admin-user-name", "{props.user.name}{admin_marker}" }
            button {
                class: "pharos-admin-user-delete",
                disabled: props.disable_delete,
                onclick: {
                    let on_delete = props.on_delete;
                    move |_| on_delete.call(id_for_handler.clone())
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
        assert_eq!(AdminTab::all().len(), 4);
    }
}
