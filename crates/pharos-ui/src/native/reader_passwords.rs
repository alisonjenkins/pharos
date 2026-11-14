//! Settings → Account, "E-readers" (045, ADR 0128): the passwords an
//! e-reader such as KOReader signs in to the OPDS catalogue with. A new one
//! is shown once, beside the catalogue's URL and the username; the list
//! shows each one's label and when it was made and last used, and revokes
//! it. Pure, so it renders under `dioxus-ssr`.

use dioxus::prelude::*;
use pharos_client::api::ReaderPassword;

use crate::views::admin::date_label;

/// What the list shows while it loads, once it has, or when it failed.
#[derive(Debug, Clone, PartialEq)]
pub enum ReaderPasswordsState {
    Loading,
    Loaded(Vec<ReaderPassword>),
    Failed(String),
}

/// A password just made: shown until the viewer is done with it, and never
/// again.
#[derive(Clone, PartialEq)]
pub struct ShownPassword {
    pub label: String,
    pub password: String,
}

impl std::fmt::Debug for ShownPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShownPassword")
            .field("label", &self.label)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The catalogue's address for a reader: `origin` (this page's) and the
/// server's `/opds/` path. A page with no origin of its own (the TV app)
/// names the part the viewer fills in.
pub fn opds_url(origin: &str, path: &str) -> String {
    let origin = origin.trim_end_matches('/');
    if origin.is_empty() {
        format!("https://<your Pharos address>{path}")
    } else {
        format!("{origin}{path}")
    }
}

/// "Made 2026-11-14 · Last used 2026-11-14", from ms since the epoch.
pub fn when_label(p: &ReaderPassword) -> String {
    let made = date_label(p.created_at_ms.div_euclid(1000));
    match p.last_used_at_ms {
        Some(ms) => format!(
            "Made {made} · Last used {}",
            date_label(ms.div_euclid(1000))
        ),
        None => format!("Made {made} · Never used"),
    }
}

/// What the viewer reads when a create is refused.
pub fn create_problem(e: &pharos_client::ClientError) -> String {
    match e {
        pharos_client::ClientError::Api { kind: "limit", .. } => {
            "You have 10 e-reader passwords already. Revoke one you no longer use first.".to_owned()
        }
        pharos_client::ClientError::Api {
            kind: "invalid", ..
        } => "Use a name of up to 64 characters, such as \u{201c}Kobo\u{201d}.".to_owned(),
        e => format!("Couldn't make the password: {e}"),
    }
}

/// The section. `on_create` gets the typed label, `on_copy` the password,
/// `on_revoke` an id once its revoke is confirmed; `on_done` hides the shown
/// password. `copied` is the last copy's result.
#[component]
pub fn ReaderPasswordsView(
    state: ReaderPasswordsState,
    busy: bool,
    shown: Option<ShownPassword>,
    copied: Option<bool>,
    problem: Option<String>,
    opds_url: String,
    username: Option<String>,
    on_create: EventHandler<String>,
    on_copy: EventHandler<String>,
    on_done: EventHandler<()>,
    on_revoke: EventHandler<String>,
) -> Element {
    let mut label = use_signal(String::new);
    let mut label_problem = use_signal(|| None::<&'static str>);
    let mut confirming = use_signal(|| None::<String>);
    let passwords = match &state {
        ReaderPasswordsState::Loaded(p) => p.clone(),
        _ => Vec::new(),
    };
    rsx! {
        section { class: "pharos-settings-page pharos-reader-passwords",
            h2 { "E-readers" }
            p { class: "pharos-settings-lede",
                "An e-reader such as KOReader can browse and download your books from the catalogue at "
                code { "{opds_url}" }
                ". Give each reader its own password: it opens only the catalogue, never the rest of your account."
            }
            if let Some(shown) = &shown {
                div { class: "pharos-settings-group pharos-reader-password-new",
                    h3 { "Password for {shown.label}" }
                    p { class: "pharos-setting-help",
                        "Type it into the reader now. Pharos shows it only this once."
                    }
                    dl { class: "pharos-reader-password-details",
                        dt { "Catalogue URL" }
                        dd { code { "{opds_url}" } }
                        if let Some(name) = &username {
                            dt { "Username" }
                            dd { code { "{name}" } }
                        }
                        dt { "Password" }
                        dd {
                            code { class: "pharos-reader-password-secret", id: "reader-password-secret",
                                "{shown.password}"
                            }
                        }
                    }
                    div { class: "pharos-reader-password-actions",
                        button {
                            r#type: "button",
                            onclick: {
                                let password = shown.password.clone();
                                move |_| on_copy.call(password.clone())
                            },
                            "Copy password"
                        }
                        button {
                            r#type: "button",
                            onclick: move |_| on_done.call(()),
                            "Done"
                        }
                    }
                    match copied {
                        Some(true) => rsx! { p { class: "pharos-notice", "Copied." } },
                        Some(false) => rsx! {
                            p { class: "pharos-notice", "Couldn't copy here: select the password and copy it." }
                        },
                        None => rsx! {},
                    }
                }
            }
            form {
                class: "pharos-settings-group pharos-reader-password-form",
                onsubmit: move |evt: FormEvent| {
                    evt.prevent_default();
                    let typed = label.read().trim().to_owned();
                    if typed.is_empty() {
                        label_problem.set(Some("Name the reader, such as \u{201c}Kobo\u{201d}."));
                        return;
                    }
                    label_problem.set(None);
                    label.set(String::new());
                    on_create.call(typed);
                },
                h3 { "Add an e-reader" }
                div { class: "pharos-reader-password-form-row",
                    label {
                        "Name"
                        input {
                            id: "reader-password-label",
                            r#type: "text",
                            maxlength: "64",
                            autocomplete: "off",
                            placeholder: "Kobo",
                            value: "{label}",
                            oninput: move |e| label.set(e.value()),
                        }
                    }
                    button { r#type: "submit", disabled: busy,
                        if busy { "Making…" } else { "Make a password" }
                    }
                }
                if let Some(text) = label_problem() {
                    p { class: "pharos-error", role: "alert", "{text}" }
                }
            }
            match &state {
                ReaderPasswordsState::Loading => rsx! {
                    p { class: "pharos-notice", role: "status", "Loading your e-reader passwords…" }
                },
                ReaderPasswordsState::Failed(text) => rsx! { p { class: "pharos-error", role: "alert", "{text}" } },
                ReaderPasswordsState::Loaded(_) if passwords.is_empty() => rsx! {
                    p { class: "pharos-empty", "No e-reader has a password yet." }
                },
                ReaderPasswordsState::Loaded(_) => rsx! {
                    ul { class: "pharos-settings-group pharos-reader-password-list",
                        for p in passwords.iter() {
                            li { key: "{p.id}", class: "pharos-reader-password",
                                div { class: "pharos-reader-password-text",
                                    p { class: "pharos-setting-name", "{p.label}" }
                                    p { class: "pharos-setting-help", "{when_label(p)}" }
                                }
                                div { class: "pharos-reader-password-actions",
                                    if confirming().as_deref() == Some(p.id.as_str()) {
                                        button {
                                            r#type: "button",
                                            id: "reader-password-keep-{p.id}",
                                            onclick: {
                                                let id = p.id.clone();
                                                move |_| {
                                                    confirming.set(None);
                                                    crate::native::focus_after_render(format!("reader-password-revoke-{id}"));
                                                }
                                            },
                                            "Keep"
                                        }
                                        button {
                                            r#type: "button",
                                            class: "pharos-danger is-strong",
                                            disabled: busy,
                                            onclick: {
                                                let id = p.id.clone();
                                                move |_| {
                                                    confirming.set(None);
                                                    on_revoke.call(id.clone());
                                                }
                                            },
                                            "Revoke {p.label}"
                                        }
                                    } else {
                                        button {
                                            r#type: "button",
                                            id: "reader-password-revoke-{p.id}",
                                            class: "pharos-danger",
                                            onclick: {
                                                let id = p.id.clone();
                                                move |_| {
                                                    confirming.set(Some(id.clone()));
                                                    crate::native::focus_after_render(format!("reader-password-keep-{id}"));
                                                }
                                            },
                                            "Revoke"
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
            if let Some(text) = problem {
                p { class: "pharos-error", role: "alert", "{text}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn password(id: &str, label: &str, last_used: Option<i64>) -> ReaderPassword {
        ReaderPassword {
            id: id.into(),
            label: label.into(),
            created_at_ms: 1_700_000_000_000,
            last_used_at_ms: last_used,
        }
    }

    #[derive(Clone, PartialEq)]
    struct Props {
        state: ReaderPasswordsState,
        shown: Option<ShownPassword>,
        copied: Option<bool>,
    }

    fn render(
        state: ReaderPasswordsState,
        shown: Option<ShownPassword>,
        copied: Option<bool>,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |p: Props| {
                rsx! {
                    ReaderPasswordsView {
                        state: p.state,
                        busy: false,
                        shown: p.shown,
                        copied: p.copied,
                        problem: None,
                        opds_url: "https://pharos.example/opds/",
                        username: Some("reader".to_owned()),
                        on_create: |_| {},
                        on_copy: |_| {},
                        on_done: |_| {},
                        on_revoke: |_| {},
                    }
                }
            },
            Props {
                state,
                shown,
                copied,
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_new_password_is_shown_with_the_url_and_username() {
        let html = render(
            ReaderPasswordsState::Loaded(vec![password("abcdef", "Kobo", None)]),
            Some(ShownPassword {
                label: "Kobo".into(),
                password: "abcdef-ghjk-mnpq-rstu-vwxy".into(),
            }),
            Some(true),
        );
        assert!(html.contains("Password for Kobo"), "{html}");
        assert!(html.contains("abcdef-ghjk-mnpq-rstu-vwxy"), "{html}");
        assert!(html.contains("https://pharos.example/opds/"), "{html}");
        assert!(html.contains(">reader<"), "the username: {html}");
        assert!(html.contains("Copy password"), "{html}");
        assert!(html.contains("Copied."), "{html}");
    }

    #[test]
    fn the_list_shows_when_each_was_made_and_used_and_never_the_password() {
        let html = render(
            ReaderPasswordsState::Loaded(vec![
                password("abcdef", "Kobo", Some(1_731_542_400_000)),
                password("ghjkmn", "Tablet", None),
            ]),
            None,
            None,
        );
        assert!(
            html.contains("Made 2023-11-14 · Last used 2024-11-14"),
            "{html}"
        );
        assert!(html.contains("Made 2023-11-14 · Never used"), "{html}");
        assert_eq!(html.matches(">Revoke<").count(), 2, "{html}");
        assert!(!html.contains("Password for"), "nothing shown: {html}");
        assert!(!html.contains(":has("), "{html}");
    }

    #[test]
    fn loading_failure_and_none_say_so() {
        assert!(render(ReaderPasswordsState::Loading, None, None).contains("Loading your e-reader"));
        assert!(render(ReaderPasswordsState::Failed("down".into()), None, None).contains("down"));
        assert!(render(ReaderPasswordsState::Loaded(Vec::new()), None, None)
            .contains("No e-reader has a password yet"));
    }

    #[test]
    fn a_failed_copy_says_how_to_copy_by_hand() {
        let html = render(
            ReaderPasswordsState::Loaded(Vec::new()),
            Some(ShownPassword {
                label: "Kobo".into(),
                password: "abcdef-ghjk-mnpq-rstu-vwxy".into(),
            }),
            Some(false),
        );
        assert!(html.contains("select the password and copy it"), "{html}");
    }

    #[test]
    fn the_url_names_the_missing_origin() {
        assert_eq!(
            opds_url("https://pharos.example/", "/opds/"),
            "https://pharos.example/opds/"
        );
        assert_eq!(
            opds_url("", "/opds/"),
            "https://<your Pharos address>/opds/"
        );
    }

    #[test]
    fn a_refused_create_says_what_to_do() {
        let refused = |kind| pharos_client::ClientError::Api {
            kind,
            reason: String::new(),
            trace_id: String::new(),
        };
        assert!(create_problem(&refused("limit")).contains("Revoke one"));
        assert!(create_problem(&refused("invalid")).contains("64 characters"));
        assert!(create_problem(&refused("unavailable")).starts_with("Couldn't make"));
    }

    #[test]
    fn a_shown_password_debugs_redacted() {
        let shown = ShownPassword {
            label: "Kobo".into(),
            password: "abcdef-ghjk-mnpq-rstu-vwxy".into(),
        };
        assert!(!format!("{shown:?}").contains("ghjk"));
    }
}
