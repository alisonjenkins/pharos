//! Controls the settings pages share. Pure views, so they render under
//! `dioxus-ssr`.

use dioxus::prelude::*;

/// A choice of a few values as a row of buttons, the chosen one pressed.
/// Buttons rather than radios: arrow keys (a TV remote's D-pad) move focus
/// between them instead of changing the value. Each button's id is
/// `{id}-{value}`.
#[component]
pub fn Choices(
    id: String,
    labelled_by: String,
    #[props(default)] described_by: Option<String>,
    options: Vec<(String, String)>,
    selected: String,
    on_pick: EventHandler<String>,
) -> Element {
    rsx! {
        div {
            class: "pharos-choices",
            id: "{id}",
            role: "group",
            aria_labelledby: "{labelled_by}",
            aria_describedby: described_by,
            for (value , label) in options {
                button {
                    key: "{value}",
                    r#type: "button",
                    id: "{id}-{value}",
                    aria_pressed: if value == selected { "true" } else { "false" },
                    onclick: {
                        let value = value.clone();
                        move |_| on_pick.call(value.clone())
                    },
                    "{label}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices(selected: &str) -> String {
        let selected = selected.to_owned();
        let mut dom = VirtualDom::new_with_props(
            |selected: String| {
                rsx! {
                    Choices {
                        id: "size",
                        labelled_by: "size-label",
                        options: vec![
                            ("s".to_owned(), "Small".to_owned()),
                            ("l".to_owned(), "Large".to_owned()),
                        ],
                        selected,
                        on_pick: |_| {},
                    }
                }
            },
            selected,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn choices_press_only_the_selected_button() {
        let html = choices("l");
        assert_eq!(html.matches("aria-pressed=\"true\"").count(), 1, "{html}");
        let large = html
            .split("<button")
            .find(|b| b.contains("id=\"size-l\""))
            .unwrap_or("");
        assert!(large.contains("aria-pressed=\"true\""), "{html}");
        assert!(html.contains("role=\"group\""), "{html}");
        assert!(html.contains("aria-labelledby=\"size-label\""), "{html}");
    }

    #[test]
    fn choices_with_an_unknown_value_press_nothing() {
        let html = choices("xl");
        assert_eq!(html.matches("aria-pressed=\"true\"").count(), 0, "{html}");
    }
}
