//! The music screen's visualiser picker: one button in the middle of the top
//! strip that opens a grid of every style with a small drawing of each.

use dioxus::prelude::*;

use super::platform;
use super::visualiser::Style;

pub const TRIGGER_ID: &str = "pharos-style-trigger";
pub const PANEL_ID: &str = "pharos-style-panel";

fn tile_id(style: Style) -> String {
    format!("pharos-style-{}", style.label())
}

#[component]
pub fn StylePicker(current: Style, on_choose: EventHandler<Style>) -> Element {
    let mut open = use_signal(|| false);
    // The tile to focus once the grid is shown; it is hidden until then.
    let mut focus = use_signal(|| None::<Style>);
    use_effect(move || {
        if let Some(style) = focus() {
            platform::focus_by_id(&tile_id(style));
            focus.set(None);
        }
    });
    let mut close = move || {
        open.set(false);
        platform::focus_by_id(TRIGGER_ID);
    };
    // Escape closes the grid from the button as well as from a tile.
    let on_escape = move |evt: KeyboardEvent| {
        if open() && evt.key() == Key::Named(NamedKey::Escape) {
            evt.stop_propagation();
            evt.prevent_default();
            close();
        }
    };
    // The screen turns clicks, keys and the wheel into transport controls;
    // inside the grid they belong to the picker, and Escape goes on to it.
    let on_key = move |evt: KeyboardEvent| {
        if evt.key() != Key::Named(NamedKey::Escape) {
            evt.stop_propagation();
        }
    };
    rsx! {
        div { class: "pharos-style-picker", onkeydown: on_escape,
            button {
                id: TRIGGER_ID,
                class: "pharos-chip pharos-style-trigger",
                r#type: "button",
                title: "Choose a visualiser",
                aria_label: "Visualiser: {current.name()}",
                aria_haspopup: "true",
                aria_expanded: if open() { "true" } else { "false" },
                aria_controls: PANEL_ID,
                onclick: move |_| {
                    let show = !open();
                    open.set(show);
                    if show {
                        focus.set(Some(current));
                    }
                    platform::info(&format!("pharos: visualiser picker shown={show}"));
                },
                Thumbnail { style: current }
                span { class: "pharos-style-name", "{current.name()}" }
                svg {
                    class: "pharos-style-caret",
                    view_box: "0 0 24 24",
                    "aria-hidden": "true",
                    path { d: "M7 10l5 5 5-5z" }
                }
            }
            if open() {
                div {
                    class: "pharos-style-scrim",
                    onclick: move |evt: MouseEvent| {
                        evt.stop_propagation();
                        close();
                    },
                    onmousedown: move |evt: MouseEvent| evt.stop_propagation(),
                    oncontextmenu: move |evt: MouseEvent| evt.stop_propagation(),
                    onwheel: move |evt: WheelEvent| evt.stop_propagation(),
                }
            }
            div {
                id: PANEL_ID,
                class: "pharos-style-panel",
                role: "group",
                aria_label: "Visualiser",
                hidden: !open(),
                onclick: move |evt: MouseEvent| evt.stop_propagation(),
                onmousedown: move |evt: MouseEvent| evt.stop_propagation(),
                oncontextmenu: move |evt: MouseEvent| evt.stop_propagation(),
                onwheel: move |evt: WheelEvent| evt.stop_propagation(),
                onkeydown: on_key,
                for s in Style::ALL {
                    button {
                        id: tile_id(s),
                        class: "pharos-style-tile",
                        r#type: "button",
                        aria_pressed: if s == current { "true" } else { "false" },
                        onclick: move |_| on_choose.call(s),
                        Thumbnail { style: s }
                        span { "{s.name()}" }
                    }
                }
            }
        }
    }
}

/// A small still drawing of `style`, in the theme's accent.
#[component]
fn Thumbnail(style: Style) -> Element {
    let body = match style {
        Style::Spectrum => rsx! {
            for (i, h) in [6, 10, 15, 22, 28, 22, 15, 10, 6].iter().enumerate() {
                rect { x: "{4 + i * 6}", y: "{34 - h}", width: "4", height: "{h}", rx: "1" }
            }
        },
        Style::Scope => rsx! {
            path {
                class: "pharos-thumb-line",
                d: "M2 20 C8 6 12 6 16 20 S24 34 28 20 S36 4 40 20 S48 30 52 20 S58 12 62 20",
            }
        },
        Style::Halo => rsx! {
            circle { class: "pharos-thumb-line", cx: "32", cy: "20", r: "10" }
            for i in 0..12 {
                line {
                    class: "pharos-thumb-line pharos-thumb-faint",
                    x1: "32", y1: "6", x2: "32", y2: if i % 3 == 0 { "1" } else { "4" },
                    transform: "rotate({i * 30} 32 20)",
                }
            }
        },
        Style::Plasma => rsx! {
            circle { class: "pharos-thumb-soft", cx: "20", cy: "16", r: "14" }
            circle { class: "pharos-thumb-soft", cx: "42", cy: "24", r: "16" }
            circle { class: "pharos-thumb-soft", cx: "34", cy: "10", r: "8" }
        },
        Style::Orbit => rsx! {
            circle { class: "pharos-thumb-line", cx: "32", cy: "20", r: "7" }
            circle { class: "pharos-thumb-line pharos-thumb-faint", cx: "32", cy: "20", r: "12" }
            circle { class: "pharos-thumb-line pharos-thumb-fainter", cx: "32", cy: "20", r: "17" }
            circle { cx: "32", cy: "20", r: "2.5" }
        },
        Style::Horizon => rsx! {
            path { d: "M20 22 A12 12 0 0 1 44 22 Z" }
            path { class: "pharos-thumb-cut", d: "M20 18h24M20 20.5h24" }
            path { class: "pharos-thumb-line", d: "M0 22h64" }
            path {
                class: "pharos-thumb-line pharos-thumb-faint",
                d: "M32 22 L32 40 M32 22 L18 40 M32 22 L46 40 M32 22 L4 40 M32 22 L60 40 M0 28h64 M0 35h64",
            }
        },
        Style::Kaleidoscope => rsx! {
            for i in 0..8 {
                path {
                    class: "pharos-thumb-soft",
                    d: "M32 20 L29 6 L32 2 L35 6 Z",
                    transform: "rotate({i * 45} 32 20)",
                }
            }
            circle { cx: "32", cy: "20", r: "3" }
        },
        Style::Starfield => rsx! {
            for (x, y, r) in [(8, 8, 1.0), (18, 30, 1.5), (26, 12, 1.0), (40, 6, 1.5), (50, 28, 2.0), (58, 14, 1.0), (34, 34, 1.0), (12, 20, 1.0), (46, 18, 1.0), (32, 20, 2.5)] {
                circle { cx: "{x}", cy: "{y}", r: "{r}" }
            }
        },
        Style::Off => rsx! {
            circle { class: "pharos-thumb-line pharos-thumb-faint", cx: "32", cy: "20", r: "10" }
            path { class: "pharos-thumb-line pharos-thumb-faint", d: "M25 27 L39 13" }
        },
    };
    rsx! {
        svg {
            class: "pharos-style-thumb",
            "data-style": style.label(),
            view_box: "0 0 64 40",
            "aria-hidden": "true",
            {body}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn render(current: Style) -> String {
        fn app(current: Style) -> Element {
            rsx! { StylePicker { current, on_choose: |_| {} } }
        }
        let mut dom = VirtualDom::new_with_props(app, current);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_picker_starts_closed_and_names_the_current_style() {
        let html = render(Style::Halo);
        let (trigger, panel) = html
            .split_once(r#"id="pharos-style-panel""#)
            .expect("a panel");
        assert!(trigger.contains(r#"aria-expanded="false""#), "{trigger}");
        assert!(trigger.contains("Halo"), "{trigger}");
        // Only the panel's own tag: the thumbnails below carry aria-hidden.
        let tag = panel.split_once('>').map_or(panel, |(tag, _)| tag);
        assert!(tag.contains(" hidden"), "{tag}");
        assert!(!html.contains("pharos-style-scrim"), "{html}");
    }

    #[test]
    fn every_style_has_a_tile_with_a_drawing_and_one_is_pressed() {
        let html = render(Style::Orbit);
        for style in Style::ALL {
            let tile = format!(r#"id="{}""#, tile_id(style));
            assert!(html.contains(&tile), "{tile}");
            let thumb = format!(r#"data-style="{}""#, style.label());
            assert!(html.contains(&thumb), "{thumb}");
        }
        assert_eq!(html.matches(r#"aria-pressed="true""#).count(), 1);
    }
}
