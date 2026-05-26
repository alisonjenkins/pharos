//! Library browse view. Renders a grid of `ItemTile`s. Fetch lives at
//! the WASM entrypoint (T24 phase 2) — this component only renders the
//! props it receives and reports tile clicks via `on_play`.

use crate::api_types::LibraryItem;
use dioxus::prelude::*;

#[component]
pub fn LibraryView(items: Vec<LibraryItem>, on_play: EventHandler<String>) -> Element {
    rsx! {
        section {
            class: "pharos-library",
            if items.is_empty() {
                p { class: "pharos-empty", "Library is empty. Run `pharos scan` on the server." }
            } else {
                div {
                    class: "pharos-grid",
                    for item in items.iter() {
                        ItemTile {
                            key: "{item.id}",
                            item: item.clone(),
                            on_play: on_play,
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub fn ItemTile(item: LibraryItem, on_play: EventHandler<String>) -> Element {
    let id = item.id.clone();
    rsx! {
        article {
            class: "pharos-tile",
            "data-kind": "{item.kind.label()}",
            onclick: move |_| on_play.call(id.clone()),
            div { class: "pharos-tile-kind", "{item.kind.label()}" }
            h3 { class: "pharos-tile-title", "{item.name}" }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::api_types::ItemKind;

    fn sample(id: &str, name: &str, kind: ItemKind) -> LibraryItem {
        LibraryItem {
            id: id.into(),
            name: name.into(),
            kind,
        }
    }

    #[test]
    fn library_view_typechecks_with_empty_and_populated_props() {
        let items_empty: Vec<LibraryItem> = vec![];
        let items_full = vec![
            sample("1", "Movie One", ItemKind::Movie),
            sample("2", "Track One", ItemKind::Audio),
        ];
        // Just confirm both prop shapes resolve.
        let _ = (items_empty, items_full);
        fn _check(items: Vec<LibraryItem>, on_play: EventHandler<String>) -> Element {
            LibraryView(LibraryViewProps { items, on_play })
        }
        let _ = _check as fn(Vec<LibraryItem>, EventHandler<String>) -> Element;
    }

    #[test]
    fn item_kind_jellyfin_roundtrip() {
        assert_eq!(ItemKind::from_jellyfin_type("Movie"), ItemKind::Movie);
        assert_eq!(ItemKind::from_jellyfin_type("Audio"), ItemKind::Audio);
        assert_eq!(ItemKind::from_jellyfin_type("Episode"), ItemKind::Episode);
        assert_eq!(ItemKind::Movie.label(), "Movie");
    }
}
