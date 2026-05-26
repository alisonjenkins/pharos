//! Top-level app shell. T25 grows this with router + library browse;
//! T26 adds player; T27 adds group-session UI.

use dioxus::prelude::*;

#[component]
pub fn App() -> Element {
    rsx! {
        Layout {
            Banner { title: "pharos" }
            Placeholder {}
        }
    }
}

#[component]
fn Layout(children: Element) -> Element {
    rsx! {
        div {
            class: "pharos-app",
            {children}
        }
    }
}

#[component]
fn Banner(title: String) -> Element {
    rsx! {
        header {
            class: "pharos-banner",
            h1 { "{title}" }
        }
    }
}

#[component]
fn Placeholder() -> Element {
    rsx! {
        main {
            class: "pharos-main",
            p { "Library view lands in T25. Player lands in T26. Group session UI lands in T27." }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn app_component_exists_and_is_callable() {
        // Renderer-free smoke: just confirm the function type resolves.
        let _: fn() -> Element = App;
    }
}
