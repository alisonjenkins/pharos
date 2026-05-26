//! Search view (T53). Parity surface for jellyfin-web's
//! `/#/search.html` page. Renders a text input bound to the parent's
//! query signal + the hit tiles grouped by kind (Video / Audio).
//! Fetching lives in the WASM-side `client::web::search_hints` call;
//! this component only renders props + emits typing / play events.

use crate::api_types::ItemKind;
use crate::client::SearchHint;
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    Idle,
    Loading,
    Empty,
    Error(String),
}

#[component]
pub fn SearchView(
    query: String,
    hits: Vec<SearchHint>,
    status: SearchStatus,
    on_query: EventHandler<String>,
    on_play: EventHandler<String>,
) -> Element {
    let (videos, audio): (Vec<&SearchHint>, Vec<&SearchHint>) = hits
        .iter()
        .partition(|h| !matches!(h.kind, ItemKind::Audio));

    rsx! {
        section {
            class: "pharos-search",
            header {
                class: "pharos-search-header",
                h2 { "Search" }
                input {
                    class: "pharos-search-input",
                    r#type: "search",
                    placeholder: "Title, person, studio…",
                    autocomplete: "off",
                    value: "{query}",
                    oninput: move |ev| on_query.call(ev.value()),
                }
            }
            match status {
                SearchStatus::Loading => rsx! {
                    p { class: "pharos-search-loading", "Searching…" }
                },
                SearchStatus::Error(err) => rsx! {
                    p { class: "pharos-error", "{err}" }
                },
                SearchStatus::Empty => rsx! {
                    p { class: "pharos-empty", "No matches for \"{query}\"." }
                },
                SearchStatus::Idle => rsx! {
                    if !videos.is_empty() {
                        h3 { class: "pharos-search-group", "Video" }
                        ul {
                            class: "pharos-search-list",
                            for h in videos.iter().cloned() {
                                SearchHitRow { hit: h.clone(), on_play: on_play }
                            }
                        }
                    }
                    if !audio.is_empty() {
                        h3 { class: "pharos-search-group", "Audio" }
                        ul {
                            class: "pharos-search-list",
                            for h in audio.iter().cloned() {
                                SearchHitRow { hit: h.clone(), on_play: on_play }
                            }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn SearchHitRow(hit: SearchHint, on_play: EventHandler<String>) -> Element {
    let id = hit.id.clone();
    rsx! {
        li {
            class: "pharos-search-hit",
            "data-kind": "{hit.kind.label()}",
            onclick: move |_| on_play.call(id.clone()),
            span { class: "pharos-search-hit-kind", "{hit.kind.label()}" }
            span { class: "pharos-search-hit-name", "{hit.name}" }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn status_value_semantics() {
        let a = SearchStatus::Error("x".into());
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(SearchStatus::Idle, SearchStatus::Loading);
    }

    #[test]
    fn search_view_module_exports_present() {
        fn _check(
            query: String,
            hits: Vec<SearchHint>,
            status: SearchStatus,
            on_query: EventHandler<String>,
            on_play: EventHandler<String>,
        ) -> Element {
            SearchView(SearchViewProps {
                query,
                hits,
                status,
                on_query,
                on_play,
            })
        }
        let _ = _check
            as fn(
                String,
                Vec<SearchHint>,
                SearchStatus,
                EventHandler<String>,
                EventHandler<String>,
            ) -> Element;
    }
}
