//! Remote control + casting view (T60).
//!
//! Parity for jellyfin-web's Sessions popout — lists every active
//! `RemoteSession` (from `/Sessions`) and exposes PlayState transport
//! commands (Pause/Play/Stop/Seek/Volume) the parent dispatches via the
//! server-side `POST /Sessions/{id}/Playing/{command}` route.
//!
//! Pure component: callers own the fetch + dispatch lifecycle, this
//! file just renders + emits `RemoteAction`s.

use crate::client::RemoteSession;
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    /// PlayPause / Pause / Unpause / Stop / NextTrack / PreviousTrack —
    /// PlayState transport. `arg` carries optional payload (Seek's
    /// SeekPositionTicks, SetVolume's Volume).
    PlayState {
        session_id: String,
        command: String,
        arg: serde_json::Value,
    },
    /// DisplayContent, ToggleMute, ToggleFullscreen — general commands.
    General {
        session_id: String,
        command: String,
        arg: serde_json::Value,
    },
    /// Caller refetches `/Sessions`.
    Refresh,
}

#[component]
pub fn RemoteControlView(
    sessions: Vec<RemoteSession>,
    /// Caller's own session id — the matching session can't remote-
    /// control itself, so we render the row without action buttons.
    self_session_id: Option<String>,
    status: Option<String>,
    on_action: EventHandler<RemoteAction>,
) -> Element {
    rsx! {
        section {
            class: "pharos-remote",
            header {
                class: "pharos-remote-header",
                h2 { "Remote control" }
                button {
                    class: "pharos-remote-refresh",
                    onclick: move |_| on_action.call(RemoteAction::Refresh),
                    "Refresh"
                }
            }
            if let Some(s) = status.as_ref() {
                p { class: "pharos-remote-status", "{s}" }
            }
            if sessions.is_empty() {
                p { class: "pharos-empty pharos-remote-empty", "No active sessions" }
            } else {
                ul {
                    class: "pharos-remote-sessions",
                    for s in sessions.iter().cloned() {
                        SessionRow {
                            key: "{s.id}",
                            session: s.clone(),
                            is_self: self_session_id.as_deref() == Some(s.id.as_str()),
                            on_action: on_action,
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SessionRow(
    session: RemoteSession,
    is_self: bool,
    on_action: EventHandler<RemoteAction>,
) -> Element {
    let now_playing_label = match (session.now_playing_item_id.as_deref(), session.is_paused) {
        (Some(item), true) => format!("paused on {item}"),
        (Some(item), false) => format!("playing {item}"),
        (None, _) => "idle".to_string(),
    };
    let id_for_pause = session.id.clone();
    let id_for_play = session.id.clone();
    let id_for_stop = session.id.clone();
    let id_for_mute = session.id.clone();
    let id_for_next = session.id.clone();
    let id_for_prev = session.id.clone();

    rsx! {
        li {
            class: if is_self {
                "pharos-remote-session pharos-remote-session-self"
            } else {
                "pharos-remote-session"
            },
            "data-session-id": "{session.id}",
            header {
                class: "pharos-remote-session-header",
                span { class: "pharos-remote-session-user", "{session.user_name}" }
                " · "
                span { class: "pharos-remote-session-device", "{session.device_name}" }
                " · "
                span { class: "pharos-remote-session-client", "{session.client}" }
                if is_self { " · this device" }
            }
            p {
                class: "pharos-remote-session-now-playing",
                "{now_playing_label}"
            }
            if !is_self {
                div {
                    class: "pharos-remote-session-actions",
                    button {
                        class: "pharos-remote-action-pause",
                        onclick: move |_| on_action.call(RemoteAction::PlayState {
                            session_id: id_for_pause.clone(),
                            command: "Pause".into(),
                            arg: serde_json::json!({}),
                        }),
                        "Pause"
                    }
                    button {
                        class: "pharos-remote-action-play",
                        onclick: move |_| on_action.call(RemoteAction::PlayState {
                            session_id: id_for_play.clone(),
                            command: "Unpause".into(),
                            arg: serde_json::json!({}),
                        }),
                        "Play"
                    }
                    button {
                        class: "pharos-remote-action-stop",
                        onclick: move |_| on_action.call(RemoteAction::PlayState {
                            session_id: id_for_stop.clone(),
                            command: "Stop".into(),
                            arg: serde_json::json!({}),
                        }),
                        "Stop"
                    }
                    button {
                        class: "pharos-remote-action-prev",
                        onclick: move |_| on_action.call(RemoteAction::PlayState {
                            session_id: id_for_prev.clone(),
                            command: "PreviousTrack".into(),
                            arg: serde_json::json!({}),
                        }),
                        "⏮"
                    }
                    button {
                        class: "pharos-remote-action-next",
                        onclick: move |_| on_action.call(RemoteAction::PlayState {
                            session_id: id_for_next.clone(),
                            command: "NextTrack".into(),
                            arg: serde_json::json!({}),
                        }),
                        "⏭"
                    }
                    button {
                        class: "pharos-remote-action-mute",
                        onclick: move |_| on_action.call(RemoteAction::General {
                            session_id: id_for_mute.clone(),
                            command: "ToggleMute".into(),
                            arg: serde_json::json!({}),
                        }),
                        "Mute"
                    }
                }
                // Volume slider — emits SetVolume on commit. 0-100
                // matches Jellyfin's GeneralCommand SetVolume contract.
                VolumeSlider {
                    session_id: session.id.clone(),
                    on_action: on_action,
                }
                // Seek input — accepts seconds, converts to ticks
                // (10_000_000 ticks/sec) and emits PlayState Seek.
                SeekInput {
                    session_id: session.id.clone(),
                    on_action: on_action,
                }
            }
        }
    }
}

#[component]
fn VolumeSlider(session_id: String, on_action: EventHandler<RemoteAction>) -> Element {
    let mut volume = use_signal(|| 80u32);
    let id_for_commit = session_id;
    rsx! {
        label {
            class: "pharos-remote-volume",
            "Volume: {volume.read()}"
            input {
                r#type: "range",
                min: "0",
                max: "100",
                value: "{volume.read()}",
                oninput: move |ev| {
                    let v: u32 = ev.value().parse().unwrap_or(0);
                    volume.set(v.min(100));
                },
                onchange: move |_| {
                    on_action.call(RemoteAction::General {
                        session_id: id_for_commit.clone(),
                        command: "SetVolume".into(),
                        arg: serde_json::json!({ "Volume": *volume.read() }),
                    });
                },
            }
        }
    }
}

#[component]
fn SeekInput(session_id: String, on_action: EventHandler<RemoteAction>) -> Element {
    let mut seconds = use_signal(String::new);
    let id_for_submit = session_id;
    rsx! {
        form {
            class: "pharos-remote-seek",
            onsubmit: move |ev: FormEvent| {
                ev.prevent_default();
                let raw = seconds.read().clone();
                let Ok(s) = raw.trim().parse::<u64>() else {
                    return;
                };
                let ticks: u64 = s.saturating_mul(10_000_000);
                on_action.call(RemoteAction::PlayState {
                    session_id: id_for_submit.clone(),
                    command: "Seek".into(),
                    arg: serde_json::json!({ "SeekPositionTicks": ticks }),
                });
                seconds.set(String::new());
            },
            input {
                class: "pharos-remote-seek-input",
                r#type: "number",
                min: "0",
                placeholder: "Seek (s)",
                value: "{seconds.read()}",
                oninput: move |ev| seconds.set(ev.value()),
            }
            button {
                class: "pharos-remote-seek-submit",
                r#type: "submit",
                "Go"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn remote_action_value_semantics() {
        let a = RemoteAction::PlayState {
            session_id: "s1".into(),
            command: "Pause".into(),
            arg: serde_json::json!({}),
        };
        assert_eq!(a, a.clone());
        assert_ne!(a, RemoteAction::Refresh);
    }
}
