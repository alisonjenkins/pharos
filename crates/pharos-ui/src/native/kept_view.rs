//! Pure views for books kept on this device (spec 036 FR-004, FR-005): the
//! Keep offline control on a title page, and the list of kept books shared
//! by Settings and the offline start. Data in through props, events out
//! through handlers, so they render under `dioxus-ssr`.

use dioxus::prelude::*;

use super::kept::{
    size_label, total_label, AutoKeep, KeptBook, KeptStore, BUDGET_CHOICES_MIB, MAX_LAST_N,
};
use super::route::Route;
use super::views::Link;

/// The id of the Keep offline / Remove from device button.
pub const KEEP_BUTTON: &str = "pharos-keep-offline";

/// Keep offline / Remove from device, with the reason a keep was refused.
#[component]
pub fn KeepControl(
    kept: bool,
    busy: bool,
    message: Option<String>,
    on_keep: EventHandler<()>,
    on_remove: EventHandler<()>,
) -> Element {
    let (label, working) = match (kept, busy) {
        (true, false) => ("Remove from device", ""),
        (true, true) => ("Removing…", "true"),
        (false, false) => ("Keep offline", ""),
        (false, true) => ("Keeping…", "true"),
    };
    rsx! {
        button {
            id: KEEP_BUTTON,
            class: "pharos-button pharos-button-secondary",
            r#type: "button",
            disabled: busy,
            aria_busy: working,
            onclick: move |_| if kept { on_remove.call(()) } else { on_keep.call(()) },
            "{label}"
        }
        if let Some(message) = message {
            p { class: "pharos-reader-locked", role: "alert", "{message}" }
        }
    }
}

/// The id of the "Keep the book I'm reading" switch.
pub const KEEP_READING: &str = "pharos-keep-reading";
/// The id of the "Keep my last N books" switch.
pub const KEEP_RECENT: &str = "pharos-keep-recent";
/// The id of the choice of N.
pub const KEEP_LAST_N: &str = "pharos-keep-last-n";
/// The id of the choice of space for automatic books.
pub const KEEP_BUDGET: &str = "pharos-keep-budget";

fn budget_label(mib: u32) -> String {
    if mib >= 1024 && mib % 1024 == 0 {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

/// The settings of automatic keeping, per device. Each change goes out whole
/// through `on_change`.
#[component]
pub fn AutoKeepSettings(settings: AutoKeep, on_change: EventHandler<AutoKeep>) -> Element {
    let mut budgets = BUDGET_CHOICES_MIB.to_vec();
    if !budgets.contains(&settings.budget_mib) {
        budgets.push(settings.budget_mib);
        budgets.sort_unstable();
    }
    rsx! {
        div { class: "pharos-settings-group pharos-kept-settings",
            label { class: "pharos-setting",
                div { class: "pharos-setting-text",
                    span { class: "pharos-setting-name", "Keep the book I'm reading" }
                    span { class: "pharos-setting-help",
                        "Keeps the open book on this device while you read it and for 7 days after you last open it."
                    }
                }
                input {
                    r#type: "checkbox",
                    role: "switch",
                    class: "pharos-switch",
                    id: KEEP_READING,
                    checked: settings.reading,
                    onchange: move |e| on_change.call(AutoKeep { reading: e.checked(), ..settings }),
                }
            }
            label { class: "pharos-setting",
                div { class: "pharos-setting-text",
                    span { class: "pharos-setting-name", "Keep my last books" }
                    span { class: "pharos-setting-help",
                        "Keeps the books you opened most recently, up to the number chosen here."
                    }
                }
                input {
                    r#type: "checkbox",
                    role: "switch",
                    class: "pharos-switch",
                    id: KEEP_RECENT,
                    checked: settings.recent,
                    onchange: move |e| on_change.call(AutoKeep { recent: e.checked(), ..settings }),
                }
            }
            label { class: "pharos-setting",
                div { class: "pharos-setting-text",
                    span { class: "pharos-setting-name", "How many recent books" }
                }
                select {
                    id: KEEP_LAST_N,
                    disabled: !settings.recent,
                    onchange: move |e| {
                        if let Ok(last_n) = e.value().parse::<u8>() {
                            on_change.call(AutoKeep { last_n, ..settings });
                        }
                    },
                    for n in 1..=MAX_LAST_N {
                        option { value: "{n}", selected: n == settings.last_n, "{n}" }
                    }
                }
            }
            label { class: "pharos-setting",
                div { class: "pharos-setting-text",
                    span { class: "pharos-setting-name", "Space for automatic books" }
                    span { class: "pharos-setting-help",
                        "The oldest opened of these books are removed when they take more. Books you keep by hand are never removed and do not count."
                    }
                }
                select {
                    id: KEEP_BUDGET,
                    onchange: move |e| {
                        if let Ok(budget_mib) = e.value().parse::<u32>() {
                            on_change.call(AutoKeep { budget_mib, ..settings });
                        }
                    },
                    for mib in budgets {
                        option {
                            value: "{mib}",
                            selected: mib == settings.budget_mib,
                            "{budget_label(mib)}"
                        }
                    }
                }
            }
        }
    }
}

/// The kept books with their size, why each is kept, a total and a Remove
/// button for each. `Read` links open the reader, which takes the bytes
/// from the cache.
#[component]
pub fn KeptList(store: KeptStore, on_remove: EventHandler<String>) -> Element {
    if store.books.is_empty() {
        return rsx! {
            p { class: "pharos-muted", id: "pharos-kept-empty", "No books are kept on this device." }
        };
    }
    let total = total_label(&store);
    rsx! {
        p { class: "pharos-muted", id: "pharos-kept-total", "{total}" }
        ul { class: "pharos-kept-list", id: "pharos-kept-list",
            for book in store.books.iter().cloned() {
                KeptRow { key: "{book.id}", book, on_remove }
            }
        }
    }
}

#[component]
fn KeptRow(book: KeptBook, on_remove: EventHandler<String>) -> Element {
    let size = size_label(book.bytes);
    let facts = format!(
        "{} · {} · {}",
        book.format.label(),
        size,
        book.reason.label()
    );
    let id = book.id.clone();
    let name = book.title.clone();
    rsx! {
        li { class: "pharos-kept-row", "data-kept-id": "{book.id}",
            div { class: "pharos-kept-text",
                match book.item() {
                    Some(item) => rsx! {
                        Link { to: Route::Read(item), class: "pharos-kept-title", "{book.title}" }
                    },
                    None => rsx! { span { class: "pharos-kept-title", "{book.title}" } },
                }
                span { class: "pharos-muted", "{facts}" }
            }
            button {
                class: "pharos-button pharos-button-secondary",
                r#type: "button",
                aria_label: "Remove {name} from this device",
                onclick: move |_| on_remove.call(id.clone()),
                "Remove"
            }
        }
    }
}

/// What the app shows with no connection and books kept on the device.
#[component]
pub fn OfflineView(store: KeptStore, on_remove: EventHandler<String>) -> Element {
    rsx! {
        section { class: "pharos-offline", id: "pharos-offline",
            h2 { "Offline: showing books on this device" }
            p { class: "pharos-muted",
                "The server cannot be reached. Everything that needs it is unavailable until the connection returns; these books open without it."
            }
            KeptList { store, on_remove }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::super::kept::{KeptFormat, Reason};
    use super::super::views::Nav;
    use super::*;

    fn book(id: &str, title: &str, bytes: u64) -> KeptBook {
        KeptBook {
            id: id.into(),
            title: title.into(),
            format: KeptFormat::Epub,
            bytes,
            kept_at: "2026-11-10T09:00:00Z".into(),
            reason: Reason::Hand,
            locator: None,
            fraction: 0.0,
            read_at_ms: 0,
            opened_ms: 0,
        }
    }

    #[component]
    fn WithNav(children: Element) -> Element {
        use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
        children
    }

    fn render(root: fn() -> Element) -> String {
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn two_books() -> KeptStore {
        let mut store = KeptStore::new("u");
        store.keep(book("1", "Invented Tale", 1_572_864));
        store.keep(book("2", "Another Made-up Book", 2_097_152));
        store
    }

    #[test]
    fn the_list_shows_size_reason_total_and_a_remove_per_book() {
        let html =
            render(|| rsx! { WithNav { KeptList { store: two_books(), on_remove: |_| {} } } });
        assert!(html.contains("2 books · 3.5 MB"), "{html}");
        assert!(
            html.contains("Invented Tale") && html.contains("Another Made-up Book"),
            "{html}"
        );
        assert!(html.contains("EPUB · 1.5 MB · kept by hand"), "{html}");
        assert!(html.contains("href=\"/ui/read/1\""), "{html}");
        assert_eq!(html.matches(">Remove<").count(), 2, "{html}");
        assert!(
            html.contains("Remove Invented Tale from this device"),
            "{html}"
        );
    }

    #[test]
    fn an_empty_list_says_so() {
        let html = render(
            || rsx! { WithNav { KeptList { store: KeptStore::new("u"), on_remove: |_| {} } } },
        );
        assert!(html.contains("No books are kept on this device."), "{html}");
        assert!(!html.contains("pharos-kept-list"), "{html}");
    }

    #[test]
    fn the_offline_view_says_it_is_offline_and_lists_the_books() {
        let html =
            render(|| rsx! { WithNav { OfflineView { store: two_books(), on_remove: |_| {} } } });
        assert!(
            html.contains("Offline: showing books on this device"),
            "{html}"
        );
        assert!(html.contains("Invented Tale"), "{html}");
    }

    fn settings_html(settings: AutoKeep) -> String {
        let mut dom = VirtualDom::new_with_props(
            |settings: AutoKeep| rsx! { AutoKeepSettings { settings, on_change: |_| {} } },
            settings,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_settings_show_the_defaults() {
        let html = settings_html(AutoKeep::default());
        assert!(
            html.contains("Keep the book I&#39;m reading")
                || html.contains("Keep the book I'm reading"),
            "{html}"
        );
        assert!(html.contains("Space for automatic books"), "{html}");
        assert!(html.contains("Keep my last books"), "{html}");
        for size in ["128 MiB", "256 MiB", "512 MiB", "1 GiB", "2 GiB"] {
            assert!(html.contains(size), "{size} in {html}");
        }
        assert_eq!(html.matches("<option").count(), 10 + 5, "{html}");
    }

    #[test]
    fn the_switches_and_choices_show_the_settings() {
        let html = settings_html(AutoKeep {
            reading: false,
            recent: true,
            last_n: 4,
            budget_mib: 1024,
        });
        let reading = html
            .split(KEEP_READING)
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        let recent = html
            .split(KEEP_RECENT)
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(!reading.contains("checked"), "{reading}");
        assert!(recent.contains("checked"), "{recent}");
        assert!(html.contains("value=\"4\" selected"), "{html}");
        assert!(html.contains("value=\"1024\" selected"), "{html}");
        // The count is only offered while the rule is on.
        let n = html
            .split(KEEP_LAST_N)
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(!n.contains("disabled"), "{n}");
        let off = settings_html(AutoKeep::default());
        let n = off
            .split(KEEP_LAST_N)
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(n.contains("disabled"), "{n}");
    }

    #[test]
    fn a_budget_outside_the_choices_is_still_shown() {
        let html = settings_html(AutoKeep {
            budget_mib: 300,
            ..AutoKeep::default()
        });
        assert!(html.contains("value=\"300\" selected"), "{html}");
        assert!(html.contains("300 MiB"), "{html}");
    }

    #[test]
    fn each_reason_is_listed_in_words() {
        use super::super::kept::Reason;
        let mut store = KeptStore::new("u");
        store.keep(KeptBook {
            reason: Reason::Reading,
            ..book("1", "Invented Tale", 1024)
        });
        store.keep(KeptBook {
            reason: Reason::Recent,
            ..book("2", "Another Made-up Book", 1024)
        });
        let html = render_store(store);
        assert!(html.contains("EPUB · 1 kB · reading now"), "{html}");
        assert!(html.contains("EPUB · 1 kB · recent"), "{html}");
    }

    fn render_store(store: KeptStore) -> String {
        let mut dom = VirtualDom::new_with_props(
            |store: KeptStore| rsx! { WithNav { KeptList { store, on_remove: |_| {} } } },
            store,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn control(kept: bool, busy: bool, message: Option<&'static str>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(kept, busy, message): (bool, bool, Option<&'static str>)| {
                rsx! {
                    KeepControl {
                        kept,
                        busy,
                        message: message.map(str::to_owned),
                        on_keep: |_| {},
                        on_remove: |_| {},
                    }
                }
            },
            (kept, busy, message),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_button_offers_the_other_thing_than_the_book_is() {
        assert!(control(false, false, None).contains(">Keep offline<"));
        assert!(control(true, false, None).contains(">Remove from device<"));
        let busy = control(false, true, None);
        assert!(
            busy.contains("disabled") && busy.contains("Keeping…"),
            "{busy}"
        );
    }

    #[test]
    fn a_refusal_is_shown_beside_the_button() {
        let html = control(false, false, Some("This book is 90 MB, more than 64 MB."));
        assert!(
            html.contains("role=\"alert\"") && html.contains("more than 64 MB"),
            "{html}"
        );
        assert!(!control(false, false, None).contains("alert"));
    }
}
