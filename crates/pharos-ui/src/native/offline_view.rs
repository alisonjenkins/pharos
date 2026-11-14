//! Offline music screens (spec 044 T24-T26), over the `OfflineStore` port.
//! The port is registered only in the phone app; each public component draws
//! nothing without it, so web, desktop and TV render as before.

use std::sync::Arc;

use dioxus::prelude::*;

use super::offline_ui::{
    cap_from_option, cap_options, cap_selected, format_bytes, KeepState, FREE_GUARD_BYTES,
};
use super::platform;
use super::ports::{offline_store, ContainerRef, Download, OfflineError, OfflineStore};
use super::settings::Choices;

/// How often a screen re-reads the store, so a download's progress shows.
/// The port has no change feed; a read is a cheap call into the host.
const POLL_MS: u32 = 1500;

/// A store handle that props can compare: two handles are equal when they
/// are the same store.
#[derive(Clone)]
pub struct Store(pub Arc<dyn OfflineStore>);

impl PartialEq for Store {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Everything a screen shows, read in one go.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub cap: u64,
    pub used: u64,
    pub free: u64,
    pub online: bool,
    pub downloads: Vec<Download>,
}

impl Snapshot {
    pub fn read(store: &dyn OfflineStore) -> Self {
        Self {
            cap: store.cache_cap_bytes(),
            used: store.used_bytes(),
            free: store.free_bytes(),
            online: store.is_online(),
            downloads: store.downloads(),
        }
    }
}

/// A snapshot that follows the store while the component lives. Call
/// `set(Snapshot::read(..))` on it after an action to show the result at once.
pub fn use_snapshot(store: &Store) -> Signal<Snapshot> {
    let mut snap = use_signal(|| Snapshot::read(&*store.0));
    let poll = store.clone();
    use_future(move || {
        let store = poll.clone();
        async move {
            loop {
                platform::sleep_ms(POLL_MS).await;
                let now = Snapshot::read(&*store.0);
                if *snap.peek() != now {
                    snap.set(now);
                }
            }
        }
    });
    snap
}

/// Applies a cap choice. A value that is not one of the choices is refused
/// without touching the store.
pub fn apply_cap(store: &dyn OfflineStore, option: &str) -> Result<u64, String> {
    let Some(bytes) = cap_from_option(option) else {
        return Err(format!("{option:?} is not a cap this phone offers"));
    };
    store
        .set_cache_cap_bytes(bytes)
        .map(|()| bytes)
        .map_err(|e| e.to_string())
}

/// Unpins every container. Stops at the first failure and says which
/// container it was, with how many were already removed.
pub fn clear_downloads(store: &dyn OfflineStore) -> Result<usize, String> {
    let all = store.downloads();
    let mut removed = 0usize;
    for d in &all {
        store.unpin(&d.container).map_err(|e: OfflineError| {
            format!(
                "{} {:?}: {e} ({removed} of {} removed)",
                d.container.kind.label(),
                d.container.id,
                all.len()
            )
        })?;
        removed = removed.saturating_add(1);
    }
    Ok(removed)
}

/// Keeps a container for offline, or lets it go. `kept` is what the toggle
/// showed when pressed. The error names the container and why.
pub fn toggle_keep(
    store: &dyn OfflineStore,
    container: &ContainerRef,
    kept: bool,
) -> Result<(), String> {
    let result = if kept {
        store.unpin(container)
    } else {
        store.pin(container)
    };
    result.map_err(|e| format!("{} {:?}: {e}", container.kind.label(), container.id))
}

/// The "Keep for offline" toggle and its state chip, for an album or mix
/// page. Draws nothing without the offline store.
#[component]
pub fn KeepForOffline(container: ContainerRef) -> Element {
    match offline_store() {
        Some(store) => rsx! { KeepToggle { store: Store(store), container } },
        None => rsx! {},
    }
}

#[component]
pub fn KeepToggle(store: Store, container: ContainerRef) -> Element {
    let mut snap = use_snapshot(&store);
    let mut problem = use_signal(|| None::<String>);
    let state = KeepState::of(&snap().downloads, &container);
    let kept = state.is_kept();
    let label = state.label();
    let target = container.clone();
    rsx! {
        span { class: "pharos-keep-offline", "data-state": state.name(),
            button {
                class: "pharos-button pharos-button-secondary is-small",
                r#type: "button",
                aria_pressed: if kept { "true" } else { "false" },
                onclick: {
                    let store = store.clone();
                    move |_| {
                        match toggle_keep(&*store.0, &target, kept) {
                            Ok(()) => {
                                platform::info(&format!(
                                    "pharos: offline keep {} {} id={:?}",
                                    if kept { "released" } else { "requested" },
                                    target.kind.label(),
                                    target.id
                                ));
                                problem.set(None);
                            }
                            Err(e) => {
                                platform::warn(&format!("pharos: offline keep refused error={e}"));
                                problem.set(Some(e));
                            }
                        }
                        snap.set(Snapshot::read(&*store.0));
                    }
                },
                "Keep for offline"
            }
            span { class: "pharos-offline-chip", role: "status", "data-state": state.name(), "{label}" }
            if let Some(text) = problem() {
                span { class: "pharos-keep-offline-problem", role: "alert", "Couldn't keep it: {text}" }
            }
        }
    }
}

/// Settings, Offline music. Nothing without the offline store.
#[component]
pub fn OfflineMusicSettings() -> Element {
    match offline_store() {
        Some(store) => rsx! { OfflineMusicPanel { store: Store(store) } },
        None => rsx! {},
    }
}

#[component]
pub fn OfflineMusicPanel(store: Store) -> Element {
    let mut snap = use_snapshot(&store);
    let mut confirming = use_signal(|| false);
    let mut notice = use_signal(|| None::<(bool, String)>);
    let now = snap();
    let tracks: usize = now.downloads.iter().map(|d| d.tracks.len()).sum();
    let nothing_kept = now.downloads.is_empty();
    rsx! {
        section { class: "pharos-settings-page pharos-offline-music",
            h2 { "Offline music" }
            p { class: "pharos-settings-lede", "Albums and mixes kept on this phone play without a connection." }
            if let Some((error, text)) = notice() {
                p {
                    class: "pharos-notice",
                    role: if error { "alert" } else { "status" },
                    "{text}"
                }
            }
            div { class: "pharos-settings-group",
                div { class: "pharos-setting pharos-offline-cap",
                    div { class: "pharos-setting-text",
                        span { id: "offline-cap-label", class: "pharos-setting-name", "Storage cap" }
                        span { id: "offline-cap-help", class: "pharos-setting-help",
                            "The most space kept music may use."
                        }
                    }
                    Choices {
                        id: "offline-cap",
                        labelled_by: "offline-cap-label",
                        described_by: "offline-cap-help",
                        options: cap_options(),
                        selected: cap_selected(now.cap),
                        on_pick: {
                            let store = store.clone();
                            move |option: String| {
                                match apply_cap(&*store.0, &option) {
                                    Ok(bytes) => {
                                        platform::info(&format!("pharos: offline cap set bytes={bytes}"));
                                        notice.set(None);
                                    }
                                    Err(e) => {
                                        platform::warn(&format!(
                                            "pharos: offline cap refused option={option:?} error={e}"
                                        ));
                                        notice.set(Some((true, format!("The cap was not changed: {e}"))));
                                    }
                                }
                                snap.set(Snapshot::read(&*store.0));
                            }
                        },
                    }
                }
                div { class: "pharos-setting",
                    div { class: "pharos-setting-text",
                        span { class: "pharos-setting-name", "Used" }
                        span { id: "offline-used", class: "pharos-setting-help", "{format_bytes(now.used)} of {format_bytes(now.cap)}" }
                    }
                }
                div { class: "pharos-setting",
                    div { class: "pharos-setting-text",
                        span { class: "pharos-setting-name", "Free on this phone" }
                        span { id: "offline-free", class: "pharos-setting-help", "{format_bytes(now.free)}" }
                        span { id: "offline-guard", class: "pharos-setting-help",
                            "Downloads keep {format_bytes(FREE_GUARD_BYTES)} of the phone free."
                        }
                    }
                }
                div { class: "pharos-setting pharos-offline-clear",
                    div { class: "pharos-setting-text",
                        span { class: "pharos-setting-name", "Clear downloads" }
                        span { class: "pharos-setting-help",
                            if nothing_kept {
                                "Nothing is kept on this phone."
                            } else {
                                "Removes every kept album and mix from this phone. They stay in your library."
                            }
                        }
                    }
                    if confirming() {
                        div { class: "pharos-offline-confirm", role: "group", aria_label: "Confirm clearing downloads",
                            span { class: "pharos-setting-help", id: "offline-clear-ask",
                                "Remove {tracks} tracks ({format_bytes(now.used)})?"
                            }
                            button {
                                id: "offline-clear-yes",
                                class: "pharos-button pharos-button-danger is-strong",
                                r#type: "button",
                                aria_describedby: "offline-clear-ask",
                                onclick: {
                                    let store = store.clone();
                                    move |_| {
                                        confirming.set(false);
                                        match clear_downloads(&*store.0) {
                                            Ok(n) => {
                                                platform::info(&format!("pharos: offline cleared containers={n}"));
                                                notice.set(Some((false, "Downloads cleared.".to_owned())));
                                            }
                                            Err(e) => {
                                                platform::warn(&format!("pharos: offline clear failed error={e}"));
                                                notice.set(Some((true, format!("Couldn't clear all downloads: {e}"))));
                                            }
                                        }
                                        snap.set(Snapshot::read(&*store.0));
                                    }
                                },
                                "Yes, clear"
                            }
                            button {
                                id: "offline-clear-no",
                                class: "pharos-button pharos-button-quiet",
                                r#type: "button",
                                onclick: move |_| confirming.set(false),
                                "Keep them"
                            }
                        }
                    } else {
                        button {
                            id: "offline-clear",
                            class: "pharos-button pharos-button-danger",
                            r#type: "button",
                            disabled: nothing_kept,
                            onclick: move |_| confirming.set(true),
                            "Clear downloads"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::offline_ui::GIB;
    use crate::native::ports::fake::FakeOfflineStore;
    use crate::native::ports::{ContainerKind, ContainerRef};

    fn album(id: &str) -> ContainerRef {
        ContainerRef {
            kind: ContainerKind::Album,
            id: id.into(),
        }
    }

    fn store_with(ids: &[&str]) -> Arc<FakeOfflineStore> {
        let fake = Arc::new(FakeOfflineStore::new());
        for id in ids {
            fake.add_container(
                album(id),
                vec![(format!("{id}-1"), 1024), (format!("{id}-2"), 2048)],
            );
            assert_eq!(fake.pin(&album(id)), Ok(()));
        }
        fake
    }

    fn render_panel(fake: &Arc<FakeOfflineStore>) -> String {
        let handle: Arc<dyn OfflineStore> = fake.clone();
        let mut dom = VirtualDom::new_with_props(
            |store: Store| rsx! { OfflineMusicPanel { store } },
            Store(handle),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_cap_choice_presses_the_current_cap_and_defaults_to_four() {
        let fake = store_with(&[]);
        let html = render_panel(&fake);
        assert_eq!(html.matches("aria-pressed=\"true\"").count(), 1, "{html}");
        let four = html
            .split("<button")
            .find(|b| b.contains("id=\"offline-cap-4\""))
            .unwrap_or("");
        assert!(four.contains("aria-pressed=\"true\""), "{html}");
        for gib in ["1", "2", "4", "8", "16", "32"] {
            assert!(
                html.contains(&format!("id=\"offline-cap-{gib}\"")),
                "{gib}: {html}"
            );
        }
    }

    #[test]
    fn the_page_shows_used_free_and_the_two_gib_guard() {
        let fake = store_with(&["a"]);
        fake.set_free_bytes(10 * GIB);
        let html = render_panel(&fake);
        assert!(html.contains("3 KiB of 4 GiB"), "{html}");
        assert!(html.contains("10 GiB"), "{html}");
        assert!(html.contains("keep 2 GiB of the phone free"), "{html}");
    }

    #[test]
    fn clear_is_disabled_with_nothing_kept_and_enabled_otherwise() {
        let empty = render_panel(&store_with(&[]));
        let clear = |html: &str| {
            html.split("<button")
                .find(|b| b.contains("id=\"offline-clear\""))
                .map(str::to_owned)
                .unwrap_or_default()
        };
        assert!(clear(&empty).contains("disabled"), "{empty}");
        let kept = render_panel(&store_with(&["a"]));
        assert!(!clear(&kept).contains("disabled"), "{kept}");
        assert!(clear(&kept).contains("pharos-button-danger"), "{kept}");
    }

    #[test]
    fn a_cap_choice_reaches_the_store() {
        let fake = store_with(&[]);
        assert_eq!(apply_cap(&*fake, "8"), Ok(8 * GIB));
        assert_eq!(fake.cache_cap_bytes(), 8 * GIB);
    }

    #[test]
    fn a_choice_that_is_not_offered_leaves_the_cap_alone() {
        let fake = store_with(&[]);
        let err = apply_cap(&*fake, "3").err().unwrap_or_default();
        assert!(err.contains("\"3\""), "the value is named: {err}");
        assert_eq!(fake.cache_cap_bytes(), 4 * GIB);
    }

    #[test]
    fn clearing_unpins_every_container() {
        let fake = store_with(&["a", "b"]);
        assert_eq!(clear_downloads(&*fake), Ok(2));
        assert!(fake.downloads().is_empty());
        assert_eq!(fake.used_bytes(), 0);
    }

    #[test]
    fn clearing_nothing_is_fine() {
        let fake = store_with(&[]);
        assert_eq!(clear_downloads(&*fake), Ok(0));
    }

    fn render_toggle(fake: &Arc<FakeOfflineStore>, container: ContainerRef) -> String {
        let handle: Arc<dyn OfflineStore> = fake.clone();
        let mut dom = VirtualDom::new_with_props(
            |(store, container): (Store, ContainerRef)| rsx! { KeepToggle { store, container } },
            (Store(handle), container),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn pinned_fake(id: &str) -> Arc<FakeOfflineStore> {
        let fake = Arc::new(FakeOfflineStore::new());
        fake.add_container(
            album(id),
            vec![(format!("{id}-1"), 10), (format!("{id}-2"), 20)],
        );
        fake
    }

    #[test]
    fn an_unkept_album_shows_an_unpressed_toggle_and_not_kept() {
        let fake = pinned_fake("a");
        let html = render_toggle(&fake, album("a"));
        assert!(html.contains("aria-pressed=\"false\""), "{html}");
        assert!(html.contains("Keep for offline"), "{html}");
        assert!(html.contains("Not kept"), "{html}");
        assert!(html.contains("pharos-button-secondary"), "{html}");
    }

    #[test]
    fn a_kept_album_shows_a_pressed_toggle_and_kept() {
        let fake = pinned_fake("a");
        assert_eq!(fake.pin(&album("a")), Ok(()));
        let html = render_toggle(&fake, album("a"));
        assert!(html.contains("aria-pressed=\"true\""), "{html}");
        assert!(html.contains("data-state=\"kept\""), "{html}");
        assert!(html.contains(">Kept<"), "{html}");
    }

    #[test]
    fn a_failed_track_shows_on_the_chip_instead_of_kept() {
        let fake = pinned_fake("a");
        assert_eq!(fake.pin(&album("a")), Ok(()));
        fake.fail_track("a-2");
        let html = render_toggle(&fake, album("a"));
        assert!(html.contains("1 track failed"), "{html}");
        assert!(!html.contains(">Kept<"), "{html}");
    }

    #[test]
    fn the_toggle_pins_then_unpins() {
        let fake = pinned_fake("a");
        assert_eq!(toggle_keep(&*fake, &album("a"), false), Ok(()));
        assert!(fake.is_cached("a-1"));
        assert_eq!(toggle_keep(&*fake, &album("a"), true), Ok(()));
        assert!(!fake.is_cached("a-1"));
    }

    #[test]
    fn a_refused_pin_names_the_container_and_the_numbers() {
        let fake = pinned_fake("a");
        assert_eq!(fake.set_cache_cap_bytes(5), Ok(()));
        let err = toggle_keep(&*fake, &album("a"), false)
            .err()
            .unwrap_or_default();
        assert!(err.contains("album \"a\""), "{err}");
        assert!(err.contains("30") && err.contains('5'), "{err}");
    }

    #[test]
    fn a_mix_is_pinned_under_its_own_kind() {
        let fake = Arc::new(FakeOfflineStore::new());
        let mix = ContainerRef {
            kind: ContainerKind::Mix,
            id: "Morning".into(),
        };
        fake.add_container(mix.clone(), vec![("m-1".into(), 10)]);
        assert_eq!(toggle_keep(&*fake, &mix, false), Ok(()));
        let html = render_toggle(&fake, mix);
        assert!(html.contains(">Kept<"), "{html}");
        assert_eq!(
            render_toggle(&fake, album("Morning"))
                .matches("Not kept")
                .count(),
            1
        );
    }

    #[test]
    fn without_a_registered_store_the_toggle_draws_nothing() {
        let mut dom = VirtualDom::new(
            || rsx! { KeepForOffline { container: ContainerRef { kind: ContainerKind::Album, id: "a".into() } } },
        );
        dom.rebuild_in_place();
        assert_eq!(dioxus_ssr::render(&dom), "");
    }

    #[test]
    fn without_a_registered_store_the_settings_section_draws_nothing() {
        let mut dom = VirtualDom::new(|| rsx! { OfflineMusicSettings {} });
        dom.rebuild_in_place();
        assert_eq!(dioxus_ssr::render(&dom), "");
    }
}
