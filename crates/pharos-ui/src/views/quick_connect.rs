//! QuickConnect UI (T63 follow-on).
//!
//! Two pure components:
//! - `QuickConnectGuestView` — pre-login screen showing the 6-digit
//!   `Code` while the parent polls `/QuickConnect/Connect` against
//!   the `Secret`. On `Authenticated:true` the parent stows the new
//!   AccessToken in the user signal.
//! - `QuickConnectAuthorizeView` — authenticated-side input where a
//!   signed-in user pastes the code they're vouching for.

use crate::client::QuickConnectInitiate;
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickConnectGuestAction {
    /// User requested a fresh `/QuickConnect/Initiate` round-trip.
    Initiate,
    /// Cancel + back out to the password login form.
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickConnectGuestStatus {
    Idle,
    Pending,
    /// Code expired or server rejected — surface text.
    Error(String),
}

#[component]
pub fn QuickConnectGuestView(
    pending: Option<QuickConnectInitiate>,
    status: QuickConnectGuestStatus,
    on_action: EventHandler<QuickConnectGuestAction>,
) -> Element {
    rsx! {
        section {
            class: "pharos-qc-guest",
            header { class: "pharos-qc-guest-header", h1 { "Quick Connect" } }
            if let Some(p) = pending.as_ref() {
                p {
                    class: "pharos-qc-guest-instructions",
                    "On a signed-in device, open Quick Connect and enter:"
                }
                p {
                    class: "pharos-qc-guest-code",
                    code { "{p.code}" }
                }
                p {
                    class: "pharos-qc-guest-poll",
                    match status {
                        QuickConnectGuestStatus::Pending => rsx! { "Waiting for approval…" },
                        QuickConnectGuestStatus::Idle => rsx! { "Ready" },
                        QuickConnectGuestStatus::Error(ref e) => rsx! {
                            span { class: "pharos-error", "{e}" }
                        },
                    }
                }
            } else {
                p {
                    class: "pharos-qc-guest-empty",
                    "Click Start to request a code."
                }
            }
            div {
                class: "pharos-qc-guest-actions",
                button {
                    class: "pharos-qc-guest-start",
                    onclick: move |_| on_action.call(QuickConnectGuestAction::Initiate),
                    if pending.is_some() { "New code" } else { "Start" }
                }
                button {
                    class: "pharos-qc-guest-cancel",
                    onclick: move |_| on_action.call(QuickConnectGuestAction::Cancel),
                    "Back to password sign-in"
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickConnectAuthorizeAction {
    Submit { code: String },
}

#[component]
pub fn QuickConnectAuthorizeView(
    status: Option<String>,
    on_action: EventHandler<QuickConnectAuthorizeAction>,
) -> Element {
    let mut code = use_signal(String::new);
    let mut code_for_submit = code;
    rsx! {
        section {
            class: "pharos-qc-authorize",
            header { class: "pharos-qc-authorize-header", h1 { "Quick Connect" } }
            p {
                class: "pharos-qc-authorize-instructions",
                "Type the 6-digit code shown on the signing-in device:"
            }
            form {
                class: "pharos-qc-authorize-form",
                onsubmit: move |ev: FormEvent| {
                    ev.prevent_default();
                    let c = code_for_submit.read().trim().to_string();
                    if c.is_empty() {
                        return;
                    }
                    on_action.call(QuickConnectAuthorizeAction::Submit { code: c });
                    code_for_submit.set(String::new());
                },
                input {
                    class: "pharos-qc-authorize-input",
                    r#type: "text",
                    inputmode: "numeric",
                    maxlength: "6",
                    pattern: r#"\d{6}"#,
                    placeholder: "000000",
                    value: "{code.read()}",
                    oninput: move |ev| code.set(ev.value()),
                }
                button {
                    class: "pharos-qc-authorize-submit",
                    r#type: "submit",
                    "Authorize"
                }
            }
            if let Some(s) = status.as_ref() {
                p { class: "pharos-qc-authorize-status", "{s}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn guest_action_value_semantics() {
        let a = QuickConnectGuestAction::Initiate;
        assert_eq!(a, QuickConnectGuestAction::Initiate);
        assert_ne!(a, QuickConnectGuestAction::Cancel);
    }

    #[test]
    fn authorize_action_value_semantics() {
        let a = QuickConnectAuthorizeAction::Submit {
            code: "123456".into(),
        };
        assert_eq!(a, a.clone());
    }
}
