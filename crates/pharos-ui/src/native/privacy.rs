//! The Privacy settings page as a pure view: whether this account sends
//! diagnostics to the server. Data in through props, events out through
//! handlers, so it renders under `dioxus-ssr`.

use dioxus::prelude::*;

use super::appearance::Notice;

#[component]
pub fn PrivacyView(
    notice: Option<Notice>,
    diagnostics: bool,
    diagnostics_key: u32,
    on_diagnostics: EventHandler<bool>,
) -> Element {
    rsx! {
        section { class: "pharos-settings-page pharos-privacy-page",
            h2 { "Privacy" }
            p { class: "pharos-settings-lede", "What this account shares with the server." }
            if let Some(notice) = &notice {
                p {
                    class: "pharos-notice",
                    role: if notice.error { "alert" } else { "status" },
                    "{notice.text}"
                }
            }
            div { class: "pharos-settings-group",
                label { class: "pharos-setting pharos-diagnostics",
                    div { class: "pharos-setting-text",
                        span { class: "pharos-setting-name", "Send diagnostics to this server" }
                        span { id: "diagnostics-help", class: "pharos-setting-help",
                            "Off by default, and saved to your account on this server. When on, playback errors and theme fallbacks are sent to this server's logs. Stream URLs and credentials are never sent."
                        }
                    }
                    // Keyed so a failed save rebuilds the box: the DOM keeps the
                    // user's flip, and an unchanged `checked` never resets it.
                    input {
                        key: "{diagnostics_key}",
                        r#type: "checkbox",
                        role: "switch",
                        class: "pharos-switch",
                        id: "diagnostics-opt-in",
                        checked: diagnostics,
                        aria_describedby: "diagnostics-help",
                        onchange: move |ev| on_diagnostics.call(ev.checked()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(diagnostics: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            |diagnostics: bool| {
                rsx! {
                    PrivacyView {
                        notice: None,
                        diagnostics,
                        diagnostics_key: 0,
                        on_diagnostics: |_| {},
                    }
                }
            },
            diagnostics,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn diagnostics_box(html: &str) -> &str {
        html.find("id=\"diagnostics-opt-in\"")
            .and_then(|at| {
                let start = html[..at].rfind('<')?;
                let end = at + html[at..].find('>')?;
                html.get(start..=end)
            })
            .unwrap_or("<toggle not rendered>")
    }

    #[test]
    fn diagnostics_box_renders_the_server_value_it_is_given() {
        let on = page(true);
        assert!(
            diagnostics_box(&on).contains("checked"),
            "a server value of on must render checked: {}",
            diagnostics_box(&on)
        );
        let off = page(false);
        assert!(
            !diagnostics_box(&off).contains("checked"),
            "a server value of off must render unchecked: {}",
            diagnostics_box(&off)
        );
    }

    #[test]
    fn the_diagnostics_box_is_a_switch() {
        let html = page(false);
        assert!(diagnostics_box(&html).contains("role=\"switch\""), "{html}");
    }
}
