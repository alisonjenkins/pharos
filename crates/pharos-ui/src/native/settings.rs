//! The settings pages' frame: a sidebar of sections beside the page shown,
//! and the controls the pages share. Pure views, so they render under
//! `dioxus-ssr`.

use dioxus::prelude::*;

use super::route::Route;
use super::views::Link;

/// A settings page, as the sidebar lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Appearance,
    Playback,
    Account,
    Devices,
    Privacy,
}

impl Section {
    /// Sidebar order.
    pub const ALL: [Section; 5] = [
        Section::Appearance,
        Section::Playback,
        Section::Account,
        Section::Devices,
        Section::Privacy,
    ];

    /// The section a route shows, if it is a settings page.
    pub fn of(route: &Route) -> Option<Section> {
        match route {
            Route::Appearance => Some(Section::Appearance),
            Route::Playback => Some(Section::Playback),
            Route::Account => Some(Section::Account),
            Route::ApproveDevice => Some(Section::Devices),
            Route::Privacy => Some(Section::Privacy),
            _ => None,
        }
    }

    pub fn route(self) -> Route {
        match self {
            Section::Appearance => Route::Appearance,
            Section::Playback => Route::Playback,
            Section::Account => Route::Account,
            Section::Devices => Route::ApproveDevice,
            Section::Privacy => Route::Privacy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Playback => "Playback",
            Section::Account => "Account",
            Section::Devices => "Devices",
            Section::Privacy => "Privacy",
        }
    }

    /// The icon's SVG path, on a 24-unit stroked grid.
    fn icon(self) -> &'static str {
        match self {
            Section::Appearance => "M12 3a9 9 0 1 0 0 18a9 9 0 0 0 0-18zM12 3v18",
            Section::Playback => "M7 4.5v15l12-7.5z",
            Section::Account => "M12 12a4 4 0 1 0 0-8a4 4 0 0 0 0 8zM4 21c1-4 4.5-6 8-6s7 2 8 6",
            Section::Devices => "M3 5h18v12H3zM8 21h8M12 17v4",
            Section::Privacy => "M12 3l8 3v6c0 5-3.5 8-8 9c-4.5-1-8-4-8-9V6z",
        }
    }
}

/// A settings page inside the frame: the sections down the side, `current`
/// marked, and the page beside them. Below a tablet's width the sections
/// become a row of tabs above the page.
#[component]
pub fn SettingsLayout(current: Section, children: Element) -> Element {
    rsx! {
        div { class: "pharos-settings",
            nav { class: "pharos-settings-nav", aria_label: "Settings",
                p { class: "pharos-settings-title", "Settings" }
                ul {
                    for section in Section::ALL {
                        li { key: "{section.label()}",
                            Link {
                                to: section.route(),
                                class: "pharos-settings-link",
                                current: section == current,
                                svg {
                                    class: "pharos-settings-icon",
                                    view_box: "0 0 24 24",
                                    "aria-hidden": "true",
                                    path { d: section.icon() }
                                }
                                span { "{section.label()}" }
                            }
                        }
                    }
                }
            }
            div { class: "pharos-settings-panel", {children} }
        }
    }
}

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

    #[test]
    fn every_section_maps_to_its_route_and_back() {
        for section in Section::ALL {
            assert_eq!(Section::of(&section.route()), Some(section), "{section:?}");
        }
        assert_eq!(Section::of(&Route::Home), None);
    }

    #[test]
    fn the_layout_lists_every_section_and_marks_the_current_one() {
        let mut dom = VirtualDom::new(|| {
            use_context_provider(|| super::super::views::Nav(EventHandler::new(|_: Route| {})));
            rsx! {
                SettingsLayout { current: Section::Playback, p { "page body" } }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for section in Section::ALL {
            assert!(
                html.contains(&format!("href=\"{}\"", section.route().path())),
                "{section:?}: {html}"
            );
        }
        assert_eq!(html.matches("aria-current=\"page\"").count(), 1, "{html}");
        let current = html
            .split("<a ")
            .find(|a| a.contains("aria-current=\"page\""))
            .unwrap_or("");
        assert!(current.contains("/ui/settings/playback"), "{html}");
        assert!(html.contains("page body"), "{html}");
        assert!(html.contains("aria-label=\"Settings\""), "{html}");
    }

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
