//! The Approve a device page (`/settings/approve-device`): lets a signed-in
//! user enter a Quick Connect code shown on another device and authorize it.

use dioxus::prelude::*;
use pharos_client::ClientError;

/// What to tell the user when the server refused approval of a Quick Connect code.
pub fn refusal_message(error: &ClientError) -> String {
    match error {
        ClientError::Api {
            kind: "notFound", ..
        } => {
            "That code is not right, or it has expired. Check the code on the device and try again."
                .to_owned()
        }
        ClientError::Api { kind: "limit", .. } => {
            "Too many wrong codes. Wait a few minutes and try again.".to_owned()
        }
        ClientError::Api {
            kind: "invalid", ..
        } => "Enter the code shown on the device.".to_owned(),
        ClientError::Api {
            kind: "unavailable",
            ..
        } => "The server could not approve the device just now. Try again.".to_owned(),
        other => format!("Could not approve the device: {other}"),
    }
}

/// Strips whitespace from a Quick Connect code and returns it only if it
/// is exactly 6 ASCII digits.
pub fn normalise_code(raw: &str) -> Option<String> {
    let stripped: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.len() == 6 && stripped.chars().all(|c| c.is_ascii_digit()) {
        Some(stripped)
    } else {
        None
    }
}

#[component]
pub fn ApproveDeviceView(
    busy: bool,
    outcome: Option<Result<(), String>>,
    on_approve: EventHandler<String>,
) -> Element {
    let mut code = use_signal(String::new);
    let mut problem = use_signal(|| None::<&'static str>);

    use_effect(use_reactive!(|outcome| {
        if matches!(outcome, Some(Ok(()))) {
            code.set(String::new());
        }
    }));

    rsx! {
        section { class: "pharos-account-settings",
            h2 { "Approve a device" }
            p { class: "pharos-account-signed-in",
                "Open Quick Connect on the device you want to sign in, then enter the code it shows. The device signs in as you."
            }
            form {
                class: "pharos-password-form",
                id: "approve-device",
                onsubmit: move |evt: FormEvent| {
                    evt.prevent_default();
                    match normalise_code(&code.read()) {
                        None => {
                            problem.set(Some("Enter the 6-digit code shown on the device."));
                        }
                        Some(valid) => {
                            problem.set(None);
                            on_approve.call(valid);
                        }
                    }
                },
                label {
                    "Code"
                    input {
                        id: "quick-connect-code",
                        r#type: "text",
                        inputmode: "numeric",
                        autocomplete: "one-time-code",
                        maxlength: "12",
                        placeholder: "000000",
                        value: "{code}",
                        oninput: move |e| {
                            code.set(e.value());
                            problem.set(None);
                        },
                    }
                }
                if let Some(err) = problem() {
                    p { class: "pharos-error", role: "alert", "{err}" }
                } else {
                    match &outcome {
                        Some(Err(text)) => rsx! { p { class: "pharos-error", role: "alert", "{text}" } },
                        Some(Ok(())) => rsx! { p { class: "pharos-notice", role: "status", "Device approved. It will finish signing in by itself." } },
                        None => rsx! {},
                    }
                }
                button {
                    r#type: "submit",
                    disabled: busy,
                    if busy { "Approving…" } else { "Approve" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(kind: &'static str, reason: &str) -> ClientError {
        ClientError::Api {
            kind,
            reason: reason.into(),
            trace_id: String::new(),
        }
    }

    fn render(outcome: Option<Result<(), String>>, busy: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(outcome, busy): (Option<Result<(), String>>, bool)| {
                rsx! {
                    ApproveDeviceView {
                        busy,
                        outcome,
                        on_approve: |_| {},
                    }
                }
            },
            (outcome, busy),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn normalise_code_accepts_six_digits_with_or_without_spaces() {
        assert_eq!(normalise_code("123456"), Some("123456".to_owned()));
        assert_eq!(normalise_code(" 123 456 "), Some("123456".to_owned()));
        assert_eq!(normalise_code("12345"), None);
        assert_eq!(normalise_code("1234567"), None);
        assert_eq!(normalise_code("12a456"), None);
        assert_eq!(normalise_code(""), None);
    }

    #[test]
    fn refusals_read_as_what_to_do_next() {
        assert_eq!(
            refusal_message(&api("notFound", "")),
            "That code is not right, or it has expired. Check the code on the device and try again."
        );
        assert_eq!(
            refusal_message(&api("limit", "")),
            "Too many wrong codes. Wait a few minutes and try again."
        );
        assert_eq!(
            refusal_message(&api("invalid", "")),
            "Enter the code shown on the device."
        );
        assert_eq!(
            refusal_message(&api("unavailable", "")),
            "The server could not approve the device just now. Try again."
        );
        assert!(refusal_message(&api("other", "broken")).contains("Could not approve the device:"));
    }

    #[test]
    fn the_view_renders_heading_input_and_idle_state() {
        let html = render(None, false);
        assert!(html.contains("<h2>Approve a device</h2>"), "{html}");
        assert!(html.contains("id=\"quick-connect-code\""), "{html}");
        assert!(html.contains("inputmode=\"numeric\""), "{html}");
        assert!(html.contains("autocomplete=\"one-time-code\""), "{html}");
        assert!(html.contains("maxlength=\"12\""), "{html}");
        assert!(html.contains("placeholder=\"000000\""), "{html}");
        assert!(html.contains(">Approve<"), "{html}");
        assert!(!html.contains("role=\"alert\""), "{html}");
        assert!(!html.contains("role=\"status\""), "{html}");
    }

    #[test]
    fn the_view_renders_outcome_messages_and_busy_state() {
        let busy_html = render(None, true);
        assert!(busy_html.contains("Approving…"), "{busy_html}");
        assert!(busy_html.contains("disabled"), "{busy_html}");

        let ok_html = render(Some(Ok(())), false);
        assert!(
            ok_html.contains("Device approved. It will finish signing in by itself."),
            "{ok_html}"
        );
        assert!(ok_html.contains("role=\"status\""), "{ok_html}");
        assert!(ok_html.contains("class=\"pharos-notice\""), "{ok_html}");

        let err_html = render(Some(Err("Something went wrong.".to_owned())), false);
        assert!(err_html.contains("Something went wrong."), "{err_html}");
        assert!(err_html.contains("role=\"alert\""), "{err_html}");
        assert!(err_html.contains("class=\"pharos-error\""), "{err_html}");
    }
}
