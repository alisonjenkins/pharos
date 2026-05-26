//! Dioxus admin UI mirroring `api::jellyfin::admin` endpoints (T50).
//!
//! Phase 1 surface: user list with delete + create form + a one-click
//! library refresh. Policy editor + password reset land with phase 2
//! once the underlying form ergonomics warrant it. Routing is owned
//! by `RootApp` — this module exports a single `AdminView` component
//! and the value types it traffics in.

use crate::client::AdminUser;
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
}

#[derive(PartialEq, Clone, Props)]
pub struct AdminViewProps {
    pub users: Vec<AdminUser>,
    pub current_user_id: String,
    pub status: Option<String>,
    pub on_action: EventHandler<AdminAction>,
}

#[component]
pub fn AdminView(props: AdminViewProps) -> Element {
    let new_name = use_signal(String::new);
    let new_password = use_signal(String::new);
    let mut new_name_sig = new_name;
    let mut new_password_sig = new_password;

    rsx! {
        div {
            class: "pharos-admin",
            header { class: "pharos-admin-header", h2 { "Admin" } }

            if let Some(msg) = props.status.as_ref() {
                p { class: "pharos-admin-status", "{msg}" }
            }

            section {
                class: "pharos-admin-section",
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
    fn admin_view_props_value_semantics() {
        let users = vec![
            AdminUser {
                id: "1".into(),
                name: "alice".into(),
                is_admin: true,
            },
            AdminUser {
                id: "2".into(),
                name: "bob".into(),
                is_admin: false,
            },
        ];
        // Renderer-free smoke: just confirm the props type clones and
        // its fields stay PartialEq. Constructing an `EventHandler`
        // requires a Dioxus runtime so we skip that bit.
        let a = users.clone();
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(a[0].name, "alice");
        assert!(a[0].is_admin);
        assert!(!a[1].is_admin);
    }
}
