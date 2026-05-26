//! Login form. Props-driven so renderer + tests can both drive it.
//!
//! The real fetch to `POST /Users/AuthenticateByName` lives in the
//! WASM entrypoint (T24 phase 2). This component only renders the form
//! and emits a `LoginAttempt` via the `on_submit` callback.

use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAttempt {
    pub username: String,
    pub password: String,
}

#[component]
pub fn LoginForm(on_submit: EventHandler<LoginAttempt>, error: Option<String>) -> Element {
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);

    rsx! {
        form {
            class: "pharos-login",
            onsubmit: move |ev| {
                ev.prevent_default();
                on_submit.call(LoginAttempt {
                    username: username.read().clone(),
                    password: password.read().clone(),
                });
            },
            h2 { "Sign in to pharos" }
            if let Some(err) = error.as_ref() {
                p { class: "pharos-error", "{err}" }
            }
            label {
                "Username"
                input {
                    r#type: "text",
                    autocomplete: "username",
                    value: "{username}",
                    oninput: move |ev| username.set(ev.value()),
                }
            }
            label {
                "Password"
                input {
                    r#type: "password",
                    autocomplete: "current-password",
                    value: "{password}",
                    oninput: move |ev| password.set(ev.value()),
                }
            }
            button { r#type: "submit", "Sign in" }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn login_form_is_callable_with_props() {
        // Renderer-free smoke: the function pointer typechecks with the
        // expected prop shape.
        fn _check(on_submit: EventHandler<LoginAttempt>, error: Option<String>) -> Element {
            LoginForm(LoginFormProps { on_submit, error })
        }
        let _ = _check as fn(EventHandler<LoginAttempt>, Option<String>) -> Element;
    }

    #[test]
    fn login_attempt_value_semantics() {
        let a = LoginAttempt {
            username: "ali".into(),
            password: "h".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
