//! Server picker (T59 phase 1). Parity for jellyfin-web's
//! `/#/selectserver` flow — the UI lets the user manage a list of
//! known pharos / jellyfin servers and pick one to log in to.
//!
//! Storage lives in localStorage (web-only); the component itself is
//! pure — props in, `ServerPickerAction` events out. Host builds use
//! an in-memory placeholder.

use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SavedServer {
    pub server_id: String,
    pub base_url: String,
    pub name: String,
    pub last_user_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerPickerAction {
    /// Picked an existing entry — caller uses `base_url` as the API
    /// target and transitions to LoginForm.
    Select(SavedServer),
    /// Manual server-URL submission. Caller validates + reads the
    /// `/System/Info/Public` endpoint to populate the SavedServer
    /// entry, then transitions to LoginForm.
    Add(String),
    /// Drop the entry from the saved list.
    Forget(String),
}

#[component]
pub fn ServerPickerView(
    saved: Vec<SavedServer>,
    /// Default URL pre-filled in the "Add server" form. Typically
    /// the window.location.origin so existing single-server flow
    /// still works without re-entering the URL.
    default_url: String,
    status: Option<String>,
    on_action: EventHandler<ServerPickerAction>,
) -> Element {
    let mut url_draft = use_signal(|| default_url.clone());
    let _ = default_url; // captured into the signal initializer above

    rsx! {
        section {
            class: "pharos-server-picker",
            header {
                class: "pharos-server-picker-header",
                h1 { "Select server" }
            }

            if let Some(msg) = status.as_ref() {
                p { class: "pharos-server-picker-status", "{msg}" }
            }

            if saved.is_empty() {
                p {
                    class: "pharos-empty pharos-server-picker-empty",
                    "No saved servers yet — enter a URL to add one."
                }
            } else {
                ul {
                    class: "pharos-server-picker-list",
                    for s in saved.iter().cloned() {
                        ServerRow {
                            key: "{s.server_id}",
                            entry: s.clone(),
                            on_action: on_action,
                        }
                    }
                }
            }

            form {
                class: "pharos-server-picker-add",
                onsubmit: move |ev: FormEvent| {
                    ev.prevent_default();
                    let url = url_draft.read().clone();
                    if !url.trim().is_empty() {
                        on_action.call(ServerPickerAction::Add(url));
                    }
                },
                label {
                    class: "pharos-server-picker-add-label",
                    "Server URL"
                }
                input {
                    class: "pharos-server-picker-add-input",
                    r#type: "url",
                    placeholder: "https://media.example.com",
                    value: "{url_draft.read()}",
                    oninput: move |ev| url_draft.set(ev.value()),
                }
                button {
                    class: "pharos-server-picker-add-submit",
                    r#type: "submit",
                    "Add server"
                }
            }
        }
    }
}

#[component]
fn ServerRow(entry: SavedServer, on_action: EventHandler<ServerPickerAction>) -> Element {
    let entry_for_select = entry.clone();
    let id_for_forget = entry.server_id.clone();
    rsx! {
        li {
            class: "pharos-server-picker-row",
            "data-server-id": "{entry.server_id}",
            button {
                class: "pharos-server-picker-select",
                onclick: move |_| on_action.call(ServerPickerAction::Select(entry_for_select.clone())),
                span { class: "pharos-server-picker-name", "{entry.name}" }
                span { class: "pharos-server-picker-url", "{entry.base_url}" }
                if !entry.last_user_name.is_empty() {
                    span {
                        class: "pharos-server-picker-last-user",
                        "Last: {entry.last_user_name}"
                    }
                }
            }
            button {
                class: "pharos-server-picker-forget",
                onclick: move |_| on_action.call(ServerPickerAction::Forget(id_for_forget.clone())),
                "Forget"
            }
        }
    }
}

/// Read the saved server list from localStorage. Returns an empty
/// vec on first launch or on host builds where there's no Storage.
#[cfg(feature = "web")]
pub fn load_saved_servers() -> Vec<SavedServer> {
    let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else {
        return Vec::new();
    };
    let Ok(Some(raw)) = storage.get_item("pharos.servers") else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<SerializedServer>>(&raw)
        .map(|v| v.into_iter().map(SavedServer::from).collect())
        .unwrap_or_default()
}

#[cfg(not(feature = "web"))]
pub fn load_saved_servers() -> Vec<SavedServer> {
    Vec::new()
}

#[cfg(feature = "web")]
pub fn save_servers(servers: &[SavedServer]) {
    let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else {
        return;
    };
    let serial: Vec<SerializedServer> = servers
        .iter()
        .cloned()
        .map(SerializedServer::from)
        .collect();
    if let Ok(json) = serde_json::to_string(&serial) {
        let _ = storage.set_item("pharos.servers", &json);
    }
}

#[cfg(not(feature = "web"))]
pub fn save_servers(_servers: &[SavedServer]) {}

/// Persistence DTO — identical shape to `SavedServer` today but kept
/// separate so we can add server-side metadata (icon URL, last-seen
/// timestamp) without breaking the in-memory type.
#[derive(serde::Serialize, serde::Deserialize)]
struct SerializedServer {
    server_id: String,
    base_url: String,
    name: String,
    last_user_name: String,
}

impl From<SerializedServer> for SavedServer {
    fn from(s: SerializedServer) -> Self {
        Self {
            server_id: s.server_id,
            base_url: s.base_url,
            name: s.name,
            last_user_name: s.last_user_name,
        }
    }
}

impl From<SavedServer> for SerializedServer {
    fn from(s: SavedServer) -> Self {
        Self {
            server_id: s.server_id,
            base_url: s.base_url,
            name: s.name,
            last_user_name: s.last_user_name,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn server_picker_action_value_semantics() {
        let s = SavedServer {
            server_id: "srv-1".into(),
            base_url: "https://media.example.com".into(),
            name: "Home".into(),
            last_user_name: "ali".into(),
        };
        let a = ServerPickerAction::Select(s.clone());
        assert_eq!(a, a.clone());
        assert_ne!(a, ServerPickerAction::Forget("srv-1".into()));
    }

    #[test]
    fn host_build_load_returns_empty_vec() {
        assert!(load_saved_servers().is_empty());
        // No-op save shouldn't panic.
        save_servers(&[]);
    }
}
