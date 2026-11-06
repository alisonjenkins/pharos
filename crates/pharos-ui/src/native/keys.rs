//! The key layer (spec 029): one page-wide listener that runs the
//! [`keymap`] bindings of the current mode, and the which-key panel that
//! lists them.

use dioxus::prelude::*;

use super::keymap::{self, Action, HelpRow, Mode, Place, Prefix, Resolved};
use super::platform;
use super::route::Route;
use super::views::Nav;

/// How long a begun sequence waits before its panel opens: typed without
/// a pause, a sequence runs without the panel.
const HELP_AFTER_MS: u32 = 500;
/// How long a begun sequence waits for its next key at all.
const SEQUENCE_LAPSES_MS: u32 = 2_000;

thread_local! {
    /// Whether a sequence (`g` …) waits for its next key.
    static PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a begun sequence waits for its next key: a player's own key
/// handler, which hears a key before the layer, leaves that key to the
/// layer, so `g h` goes home rather than seeking back (B617).
pub fn sequence_pending() -> bool {
    PENDING.get()
}

/// The mode a page asked for, with the token of the page that asked, so a
/// page leaving does not clear the mode the next page already set.
#[derive(Clone, Copy)]
pub struct KeyMode(Signal<Option<(u32, Mode)>>);

/// Provide the key mode, at the app's root.
pub fn provide_key_mode() {
    use_context_provider(|| KeyMode(Signal::new(None)));
}

/// Put the keys in `mode` while the calling page is shown.
pub fn use_key_mode(mode: Mode) {
    let Some(KeyMode(mut at)) = try_use_context::<KeyMode>() else {
        return;
    };
    let token = use_hook(|| {
        thread_local! {
            static NEXT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        NEXT.with(|n| {
            let t = n.get().wrapping_add(1);
            n.set(t);
            t
        })
    });
    use_effect(use_reactive!(|mode| at.set(Some((token, mode)))));
    use_drop(move || {
        if at.peek().is_some_and(|(t, _)| t == token) {
            at.set(None);
        }
    });
}

/// Focus the element `id` when `key` changes, if the focus is on nothing:
/// the page's main action on arrival, without scrolling.
pub fn use_arrival_focus<K: PartialEq + Clone + 'static>(key: K, id: &'static str) {
    use_effect(use_reactive!(|key| {
        let _ = &key;
        platform::focus_arrival(id);
    }));
}

/// What the panel shows.
#[derive(Debug, Clone, PartialEq)]
enum Help {
    /// The keys that can follow a prefix.
    After(Prefix),
    /// Every key of the mode.
    All,
}

/// The page-wide key listener and its help panel.
#[component]
pub fn KeyLayer(route: Route, on_palette: EventHandler) -> Element {
    let nav = use_context::<Nav>();
    let KeyMode(asked) = use_context::<KeyMode>();
    let mut at = use_signal(|| route.clone());
    use_effect(use_reactive!(|route| at.set(route)));
    let mode = use_memo(move || match &*at.read() {
        // The music player shares the watch route and asks for its own.
        Route::Watch(_) => match *asked.read() {
            Some((_, m @ (Mode::Music | Mode::Queue))) => m,
            _ => Mode::Player,
        },
        // A song playing in the mini bar or floating window keeps the
        // page's keys.
        _ => match *asked.read() {
            Some((_, Mode::Music | Mode::Queue)) | None => Mode::Browse,
            Some((_, m)) => m,
        },
    });
    let mut chord = use_signal(Vec::<String>::new);
    let mut help = use_signal(|| None::<Help>);
    // Each key bumps it, so a timer started for an older key stands down.
    let mut generation = use_signal(|| 0u32);
    let mut reset = move || {
        PENDING.set(false);
        chord.write().clear();
        help.set(None);
    };
    let mut run = move |action: Action| -> bool {
        match action {
            Action::Native | Action::Transport(_) => false,
            Action::Move(dir) => platform::move_focus(dir),
            Action::Activate => platform::activate_focused(),
            Action::Help => {
                help.set(Some(Help::All));
                true
            }
            Action::Palette => {
                on_palette.call(());
                true
            }
            Action::Go(place) => {
                nav.0.call(match place {
                    Place::Home => Route::Home,
                    Place::Together => Route::Together,
                    Place::Settings => Route::Appearance,
                });
                true
            }
            Action::Press(name) => {
                platform::press_key_action(name)
                    || fallback(name).is_some_and(platform::press_key_action)
            }
            Action::Season(step) => platform::step_tab(step),
            Action::Focus(name) => {
                if platform::focus_key_area(name) {
                    return true;
                }
                // A hidden area is drawn on the next render.
                let shown = platform::press_key_action(name);
                if shown {
                    platform::spawn_detached(async move {
                        platform::sleep_ms(0).await;
                        platform::focus_key_area(name);
                    });
                }
                shown
            }
        }
    };
    use_hook(move || {
        std::rc::Rc::new(platform::on_mode_key(move |key| {
            let open = help.peek().clone();
            if open.is_some() && key == "Escape" {
                reset();
                return true;
            }
            // An open dialog's Escape is its own (the queue's Clear).
            if key == "Escape" && platform::dialog_open() {
                return false;
            }
            let mut keys = chord.peek().clone();
            keys.push(key.to_owned());
            let typed: Vec<&str> = keys.iter().map(String::as_str).collect();
            generation += 1;
            match keymap::resolve(focus_mode(*mode.peek()), &typed) {
                // A player's keys are its own handler's, which hears only
                // keys pressed inside it: one pressed with the focus
                // elsewhere on the page is sent on to it (B618).
                Resolved::Run(Action::Transport(_)) => {
                    reset();
                    platform::forward_to_player(key)
                }
                Resolved::Run(action) => {
                    reset();
                    run(action)
                }
                Resolved::Pending(prefix) => {
                    PENDING.set(true);
                    chord.set(keys);
                    help.set(None);
                    let asked = *generation.peek();
                    // The listener runs outside the Dioxus runtime, so the
                    // timer is a plain browser task; the layer may be gone
                    // (signed out) when it fires.
                    platform::spawn_detached(async move {
                        let current = move || generation.try_peek().is_ok_and(|g| *g == asked);
                        platform::sleep_ms(HELP_AFTER_MS).await;
                        if !current() {
                            return;
                        }
                        help.set(Some(Help::After(prefix)));
                        platform::sleep_ms(SEQUENCE_LAPSES_MS - HELP_AFTER_MS).await;
                        if current() {
                            reset();
                        }
                    });
                    true
                }
                // A key that ends no sequence ends the one begun, and is
                // taken so a stray letter does nothing after `g`.
                Resolved::Unbound => {
                    let begun = keys.len() > 1 || open.is_some();
                    reset();
                    begun
                }
            }
        }))
    });
    let shown = help.read().clone();
    let Some(shown) = shown else {
        return rsx! {};
    };
    let mode = focus_mode(mode());
    let (title, rows) = match &shown {
        Help::After(p) => (
            format!("{} · {}", p.label, mode.label()),
            keymap::help(mode, Some(p.key), offered),
        ),
        Help::All => (
            format!("Keys · {}", mode.label()),
            keymap::help(mode, None, offered),
        ),
    };
    let prefix = match &shown {
        Help::After(p) => Some(keymap::key_name(p.key).to_owned()),
        Help::All => None,
    };
    rsx! {
        KeyHelp { title, prefix, rows, on_close: move |_| reset() }
    }
}

/// `mode`, or the queue's while the focus is in the music player's queue:
/// read at each key, since a removed row takes the focus without a
/// `focusout`.
fn focus_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Music | Mode::Queue if platform::focus_in_area("queue") => Mode::Queue,
        Mode::Queue => Mode::Music,
        m => m,
    }
}

/// Whether the page shows what `action` acts on, so the help lists only
/// keys that do something here: a film has no season tabs, and nothing
/// playing has no mini bar.
fn offered(action: Action) -> bool {
    let shown = platform::key_action_shown;
    match action {
        Action::Press(name) => shown(name) || fallback(name).is_some_and(shown),
        Action::Season(_) => platform::key_tabs_shown(),
        Action::Focus(name) => platform::key_area_shown(name) || shown(name),
        _ => true,
    }
}

/// The control a key presses when the page has no control of its own for
/// it: from the start is the only start when nothing is part-watched.
fn fallback(name: &str) -> Option<&'static str> {
    match name {
        "restart" => Some("play"),
        "together-start" => Some("together"),
        _ => None,
    }
}

/// The which-key panel: what the keys do here, grouped.
#[component]
pub fn KeyHelp(
    title: String,
    /// The prefix typed, shown before each key that can follow it.
    prefix: Option<String>,
    rows: Vec<HelpRow>,
    on_close: EventHandler,
) -> Element {
    let mut groups: Vec<(&'static str, Vec<HelpRow>)> = Vec::new();
    for row in rows {
        match groups.last_mut() {
            Some((label, list)) if *label == row.group.label() => list.push(row),
            _ => groups.push((row.group.label(), vec![row])),
        }
    }
    rsx! {
        aside { class: "pharos-key-help", role: "dialog", aria_label: "{title}",
            header { class: "pharos-key-help-head",
                h2 { "{title}" }
                button {
                    r#type: "button",
                    class: "pharos-key-help-close",
                    "aria-label": "Close",
                    onclick: move |_| on_close.call(()),
                    "Esc"
                }
            }
            div { class: "pharos-key-help-groups",
                for (label, list) in groups {
                    section { key: "{label}", class: "pharos-key-help-group",
                        h3 { "{label}" }
                        dl {
                            for row in list {
                                div { key: "{row.label}", class: "pharos-key-help-row",
                                    dt {
                                        for (i, k) in row.keys.iter().enumerate() {
                                            if i > 0 { " " }
                                            if let Some(p) = &prefix {
                                                span { class: "pharos-key-help-prefix", "{p} " }
                                            }
                                            kbd { "{k}" }
                                        }
                                    }
                                    dd { "{row.label}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
