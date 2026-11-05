//! The Account page (`/settings/account`) as a pure view: your avatar over
//! `Session.setAvatar` / `removeAvatar`, your password over
//! `Session.changePassword`, this device's playback choices, and the language
//! show descriptions are shown in (ADR-0062).

use dioxus::prelude::*;
use pharos_client::api::{Me, PASSWORD_CHANGED_REVOKE_FAILED};
use pharos_client::ClientError;

use super::account::Avatar;
use super::playback::{parse_crossfade, CROSSFADE_CHOICES};

/// Largest picture sent as an avatar: the server's `setAvatar` cap, under
/// one WebSocket message. A bigger one is scaled down in the browser first.
pub const MAX_AVATAR_BYTES: usize = 768 * 1024;

/// What to tell the user when the server would not take a picture.
pub fn avatar_refusal_message(error: &ClientError) -> String {
    match error {
        ClientError::Api {
            kind: "invalid",
            reason,
            ..
        } => format!("That picture cannot be used: {reason}."),
        ClientError::Api { kind: "limit", .. } => "That picture is too large.".to_owned(),
        ClientError::Api {
            kind: "unavailable",
            ..
        } => "The server could not save your picture just now. Try again.".to_owned(),
        other => format!("Could not change your picture: {other}"),
    }
}

/// The `Prefs` key the server maps onto the user's Jellyfin
/// `DescriptionLanguagePreference`; matches the server's allow-list.
pub const DESCRIPTION_LANGUAGE_PREF: &str = "description.language";

/// Saves of one pref, one at a time: a pick made while a write is in flight
/// waits for it and then writes, so the server always ends on the newest
/// pick, and a failed write puts back the last value the server confirmed,
/// never an older one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPref {
    saved: String,
    in_flight: bool,
    queued: Option<String>,
}

impl SerialPref {
    /// Starts from the value the server holds.
    pub fn new(saved: String) -> Self {
        Self {
            saved,
            in_flight: false,
            queued: None,
        }
    }

    /// The viewer picked `value`: the value to write now, or `None` when it
    /// waits behind the write in flight (a newer pick replaces it).
    pub fn pick(&mut self, value: String) -> Option<String> {
        if self.in_flight {
            self.queued = Some(value);
            None
        } else {
            self.in_flight = true;
            Some(value)
        }
    }

    /// The write of `written` finished: the next value to write, or `None`
    /// once every pick is written.
    pub fn finished(&mut self, written: String, ok: bool) -> Option<String> {
        if ok {
            self.saved = written;
        }
        let next = self.queued.take();
        self.in_flight = next.is_some();
        next
    }

    /// The last value the server confirmed.
    pub fn saved(&self) -> &str {
        &self.saved
    }
}

/// The description languages offered, as (stored value, label). Empty is the
/// server default, English; `OriginalLanguage` is whatever the show was made in.
pub const DESCRIPTION_LANGUAGES: [(&str, &str); 10] = [
    ("", "English (default)"),
    ("jpn", "Japanese"),
    ("fra", "French"),
    ("deu", "German"),
    ("spa", "Spanish"),
    ("ita", "Italian"),
    ("por", "Portuguese"),
    ("kor", "Korean"),
    ("zho", "Chinese"),
    ("OriginalLanguage", "Original language"),
];

/// How the last change went: `Ok` shows the confirmation, `Err` the message.
pub type Outcome = Result<(), String>;

/// What to tell the user when the server refused a change.
pub fn refusal_message(error: &ClientError) -> String {
    match error {
        ClientError::Api {
            kind: "notPermitted",
            ..
        } => "Your current password is not right.".to_owned(),
        ClientError::Api { kind: "limit", .. } => {
            "Too many wrong attempts. Wait a few minutes and try again.".to_owned()
        }
        ClientError::Api {
            kind: "invalid", ..
        } => "Enter a new password.".to_owned(),
        // The new password is stored, but other sessions are still signed in.
        ClientError::Api {
            kind: "unavailable",
            reason,
            ..
        } if reason == PASSWORD_CHANGED_REVOKE_FAILED => {
            "Your password changed, but your other devices could not be signed out. \
             Change it again with the new password to retry."
                .to_owned()
        }
        ClientError::Api {
            kind: "unavailable",
            ..
        } => "The server could not change your password just now. Try again.".to_owned(),
        other => format!("Could not change your password: {other}"),
    }
}

/// Checks the form before it is sent: `None` when it may go.
pub fn form_problem(current: &str, new: &str, confirm: &str) -> Option<&'static str> {
    if current.is_empty() {
        Some("Enter your current password.")
    } else if new.is_empty() {
        Some("Enter a new password.")
    } else if new != confirm {
        Some("The new passwords do not match.")
    } else {
        None
    }
}

#[component]
pub fn AccountView(
    me: Option<Me>,
    busy: bool,
    outcome: Option<Outcome>,
    avatar_busy: bool,
    avatar_outcome: Option<Outcome>,
    on_change_password: EventHandler<(String, String)>,
    on_pick_avatar: EventHandler<()>,
    on_remove_avatar: EventHandler<()>,
    /// The crossfade between songs, in seconds; 0 is off.
    #[props(default)]
    crossfade_s: u32,
    #[props(default)] on_crossfade: EventHandler<u32>,
    /// The stored description language: a value from
    /// [`DESCRIPTION_LANGUAGES`], or empty for the default.
    #[props(default)]
    description_language: String,
    #[props(default)] on_description_language: EventHandler<String>,
) -> Element {
    let name = me.as_ref().map(|me| me.name.clone());
    let has_avatar = me.as_ref().is_some_and(|me| me.avatar_url.is_some());
    let mut current = use_signal(String::new);
    let mut new = use_signal(String::new);
    let mut confirm = use_signal(String::new);
    let mut problem = use_signal(|| None::<&'static str>);
    let message = match (problem(), &outcome) {
        (Some(p), _) => Some(Err(p.to_owned())),
        (None, Some(Ok(()))) => Some(Ok(
            "Password changed. Your other devices have been signed out.".to_owned(),
        )),
        (None, Some(Err(e))) => Some(Err(e.clone())),
        (None, None) => None,
    };
    rsx! {
        section { class: "pharos-account-settings",
            h2 { "Account" }
            if let Some(name) = &name {
                p { class: "pharos-account-signed-in", "Signed in as {name}" }
            }
            div { class: "pharos-avatar-settings", id: "avatar-settings",
                h3 { "Picture" }
                div { class: "pharos-avatar-large", Avatar { me: me.clone() } }
                div { class: "pharos-avatar-actions",
                    button {
                        r#type: "button",
                        id: "avatar-choose",
                        disabled: avatar_busy,
                        onclick: move |_| on_pick_avatar.call(()),
                        if avatar_busy { "Saving…" } else if has_avatar { "Change picture" } else { "Choose a picture" }
                    }
                    if has_avatar {
                        button {
                            r#type: "button",
                            id: "avatar-remove",
                            disabled: avatar_busy,
                            onclick: move |_| on_remove_avatar.call(()),
                            "Remove"
                        }
                    }
                }
                match &avatar_outcome {
                    Some(Err(text)) => rsx! { p { class: "pharos-error", role: "alert", "{text}" } },
                    _ => rsx! {},
                }
            }
            div { class: "pharos-playback-settings", id: "playback-settings",
                h3 { "Playback" }
                label {
                    "Crossfade between songs"
                    select {
                        id: "crossfade",
                        aria_describedby: "crossfade-help",
                        onchange: move |e| on_crossfade.call(parse_crossfade(&e.value())),
                        for seconds in CROSSFADE_CHOICES {
                            option {
                                value: "{seconds}",
                                selected: seconds == crossfade_s,
                                if seconds == 0 { "Off" } else { "{seconds} seconds" }
                            }
                        }
                    }
                }
                p { id: "crossfade-help", class: "pharos-muted",
                    "Off by default. When on, the next song in the queue starts under the end of this one as it fades out. Kept on this device."
                }
            }
            div { class: "pharos-description-settings", id: "description-settings",
                h3 { "Descriptions" }
                label {
                    "Show and episode descriptions in"
                    select {
                        id: "description-language",
                        aria_describedby: "description-language-help",
                        onchange: move |e| on_description_language.call(e.value()),
                        for (value, label) in DESCRIPTION_LANGUAGES {
                            option {
                                value: "{value}",
                                selected: value == description_language,
                                "{label}"
                            }
                        }
                    }
                }
                p { id: "description-language-help", class: "pharos-muted",
                    "When a show has no description in this language, the English one is shown. Kept with your account, so every device and app uses it."
                }
            }
            form {
                class: "pharos-password-form",
                id: "change-password",
                onsubmit: move |evt: FormEvent| {
                    evt.prevent_default();
                    let (c, n) = (current.read().clone(), new.read().clone());
                    let p = form_problem(&c, &n, &confirm.read());
                    problem.set(p);
                    if p.is_none() {
                        current.set(String::new());
                        new.set(String::new());
                        confirm.set(String::new());
                        on_change_password.call((c, n));
                    }
                },
                h3 { "Change password" }
                // Lets a password manager tie the new password to the account.
                input {
                    r#type: "text",
                    name: "username",
                    autocomplete: "username",
                    value: "{name.clone().unwrap_or_default()}",
                    hidden: true,
                    readonly: true,
                }
                label {
                    "Current password"
                    input {
                        id: "password-current",
                        r#type: "password",
                        autocomplete: "current-password",
                        value: "{current}",
                        oninput: move |e| current.set(e.value()),
                    }
                }
                label {
                    "New password"
                    input {
                        id: "password-new",
                        r#type: "password",
                        autocomplete: "new-password",
                        value: "{new}",
                        oninput: move |e| new.set(e.value()),
                    }
                }
                label {
                    "Confirm new password"
                    input {
                        id: "password-confirm",
                        r#type: "password",
                        autocomplete: "new-password",
                        value: "{confirm}",
                        oninput: move |e| confirm.set(e.value()),
                    }
                }
                match message {
                    Some(Ok(text)) => rsx! { p { class: "pharos-notice", role: "status", "{text}" } },
                    Some(Err(text)) => rsx! { p { class: "pharos-error", role: "alert", "{text}" } },
                    None => rsx! {},
                }
                button { r#type: "submit", disabled: busy,
                    if busy { "Changing…" } else { "Change password" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V474: two quick picks write one after the other, and the server
    /// ends on the newer one, whichever order the answers would have come in.
    #[test]
    fn description_language_saves_run_one_at_a_time() {
        let mut p = SerialPref::new(String::new());
        assert_eq!(p.pick("fra".into()), Some("fra".into()));
        assert_eq!(p.pick("deu".into()), None, "waits for the write in flight");
        assert_eq!(
            p.pick("spa".into()),
            None,
            "a newer pick replaces the queued one"
        );
        assert_eq!(p.finished("fra".into(), true), Some("spa".into()));
        assert_eq!(p.finished("spa".into(), true), None);
        assert_eq!(p.saved(), "spa");
        // Settled: the next pick writes straight away.
        assert_eq!(p.pick("jpn".into()), Some("jpn".into()));
    }

    /// A failed write leaves the last confirmed value, not an older one.
    #[test]
    fn a_failed_description_language_save_keeps_the_last_confirmed_value() {
        let mut p = SerialPref::new("eng".into());
        assert_eq!(p.pick("fra".into()), Some("fra".into()));
        assert_eq!(p.pick("deu".into()), None);
        assert_eq!(p.finished("fra".into(), true), Some("deu".into()));
        assert_eq!(p.finished("deu".into(), false), None);
        assert_eq!(p.saved(), "fra");
    }

    fn api(kind: &'static str, reason: &str) -> ClientError {
        ClientError::Api {
            kind,
            reason: reason.into(),
            trace_id: String::new(),
        }
    }

    fn render(outcome: Option<Outcome>, busy: bool) -> String {
        render_with_language(outcome, busy, String::new())
    }

    fn render_with_language(outcome: Option<Outcome>, busy: bool, language: String) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(outcome, busy, language): (Option<Outcome>, bool, String)| {
                rsx! {
                    AccountView {
                        me: Some(Me {
                            id: "0123".to_owned(),
                            name: "ali".to_owned(),
                            is_admin: false,
                            avatar_url: None,
                        }),
                        busy,
                        outcome,
                        avatar_busy: false,
                        avatar_outcome: None,
                        on_change_password: |_| {},
                        on_pick_avatar: |_| {},
                        on_remove_avatar: |_| {},
                        description_language: language,
                    }
                }
            },
            (outcome, busy, language),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_form_asks_for_the_current_password_and_the_new_one_twice() {
        let html = render(None, false);
        for id in ["password-current", "password-new", "password-confirm"] {
            assert!(html.contains(&format!("id=\"{id}\"")), "{id}: {html}");
        }
        assert!(html.contains("autocomplete=\"current-password\""), "{html}");
        assert!(html.contains("Signed in as ali"), "{html}");
        assert!(!html.contains("role=\"alert\""), "{html}");
    }

    #[test]
    fn crossfade_is_offered_and_starts_off() {
        let html = render(None, false);
        assert!(html.contains("id=\"crossfade\""), "{html}");
        assert!(html.contains("Crossfade between songs"), "{html}");
        assert!(html.contains("<option value=\"0\" selected"), "{html}");
        assert!(html.contains("12 seconds"), "{html}");
    }

    #[test]
    fn the_description_language_is_offered_and_starts_on_english() {
        let html = render(None, false);
        assert!(html.contains("id=\"description-language\""), "{html}");
        assert!(html.contains("<option value=\"\" selected"), "{html}");
        for (value, label) in DESCRIPTION_LANGUAGES {
            assert!(
                html.contains(&format!("value=\"{value}\"")),
                "{value}: {html}"
            );
            assert!(html.contains(label), "{label}: {html}");
        }
    }

    #[test]
    fn a_stored_description_language_is_selected() {
        let html = render_with_language(None, false, "jpn".into());
        assert!(html.contains("<option value=\"jpn\" selected"), "{html}");
        assert!(!html.contains("<option value=\"\" selected"), "{html}");
    }

    #[test]
    fn without_an_avatar_the_page_offers_to_choose_one_and_shows_initials() {
        let html = render(None, false);
        assert!(html.contains("Choose a picture"), "{html}");
        assert!(!html.contains("avatar-remove"), "{html}");
        assert!(html.contains(">A<"), "{html}");
    }

    #[test]
    fn avatar_refusals_say_why() {
        assert!(
            avatar_refusal_message(&api("invalid", "not a PNG, JPEG, WebP or GIF picture"))
                .contains("not a PNG")
        );
        assert!(avatar_refusal_message(&api("limit", "avatar_bytes")).contains("too large"));
        assert!(
            avatar_refusal_message(&api("unavailable", "user store unavailable"))
                .contains("Try again")
        );
    }

    #[test]
    fn a_change_says_other_devices_were_signed_out() {
        let html = render(Some(Ok(())), false);
        assert!(html.contains("Password changed."), "{html}");
        assert!(html.contains("role=\"status\""), "{html}");
    }

    #[test]
    fn a_refusal_is_an_alert_and_busy_disables_the_button() {
        let html = render(Some(Err("nope".into())), true);
        assert!(html.contains("role=\"alert\""), "{html}");
        assert!(html.contains("Changing…"), "{html}");
        assert!(html.contains("disabled"), "{html}");
    }

    #[test]
    fn the_form_needs_every_field_and_matching_new_passwords() {
        assert_eq!(
            form_problem("", "a", "a"),
            Some("Enter your current password.")
        );
        assert_eq!(form_problem("old", "", ""), Some("Enter a new password."));
        assert_eq!(
            form_problem("old", "a", "b"),
            Some("The new passwords do not match.")
        );
        assert_eq!(form_problem("old", "a", "a"), None);
    }

    #[test]
    fn refusals_read_as_what_to_do_next() {
        assert!(
            refusal_message(&api("notPermitted", "current password is wrong"))
                .contains("current password is not right")
        );
        assert!(refusal_message(&api("limit", "login_retry_after_secs")).contains("Too many"));
        assert!(
            refusal_message(&api("invalid", "a new password is required"))
                .contains("Enter a new password")
        );
        assert!(
            refusal_message(&api("unavailable", PASSWORD_CHANGED_REVOKE_FAILED))
                .contains("Your password changed")
        );
        assert!(
            refusal_message(&api("unavailable", "user store unavailable")).contains("Try again")
        );
    }
}
