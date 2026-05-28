//! Preferences view (T55). Parity for jellyfin-web's
//! `/#/mypreferences*` panes (Display, Playback, Home, Languages).
//!
//! Single component renders three tabs over one `UserConfiguration`.
//! Pure (props in / `PrefsAction` events out) — fetch + save live in
//! the WASM-side `views::app_state::PrefsPane`.

use crate::client::{LocalizationCulture, UserConfiguration};
use dioxus::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefsTab {
    #[default]
    Display,
    Playback,
    Home,
    Languages,
}

impl PrefsTab {
    fn label(self) -> &'static str {
        match self {
            Self::Display => "Display",
            Self::Playback => "Playback",
            Self::Home => "Home",
            Self::Languages => "Languages",
        }
    }
    fn class_suffix(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Playback => "playback",
            Self::Home => "home",
            Self::Languages => "languages",
        }
    }
    fn all() -> [PrefsTab; 4] {
        [Self::Display, Self::Playback, Self::Home, Self::Languages]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefsAction {
    /// Switch active tab (caller updates its own signal).
    SelectTab(PrefsTab),
    /// Save the supplied config payload.
    Save(UserConfiguration),
}

#[component]
pub fn PrefsView(
    config: UserConfiguration,
    active_tab: PrefsTab,
    status: Option<String>,
    /// T26 phase 2 — Languages tab dropdowns are sourced from
    /// `/Localization/Cultures`. Empty falls back to the text-input
    /// shape that Playback uses.
    #[props(default)]
    cultures: Vec<LocalizationCulture>,
    on_action: EventHandler<PrefsAction>,
) -> Element {
    // Working copy edited via local signal so the form is responsive
    // without round-tripping every toggle through the parent.
    let mut draft = use_signal(|| config.clone());
    // Re-sync if the upstream config changes (eg. after Save reloads).
    use_effect({
        let config = config.clone();
        move || {
            draft.set(config.clone());
        }
    });

    rsx! {
        section {
            class: "pharos-prefs",
            "data-tab": "{active_tab.class_suffix()}",
            nav {
                class: "pharos-prefs-tabs",
                for t in PrefsTab::all() {
                    button {
                        key: "{t.class_suffix()}",
                        class: if t == active_tab {
                            "pharos-prefs-tab on"
                        } else {
                            "pharos-prefs-tab off"
                        },
                        onclick: move |_| on_action.call(PrefsAction::SelectTab(t)),
                        "{t.label()}"
                    }
                }
            }
            match active_tab {
                PrefsTab::Display => render_display(draft),
                PrefsTab::Playback => render_playback(draft),
                PrefsTab::Home => render_home(draft),
                PrefsTab::Languages => render_languages(draft, cultures.clone()),
            }
            div {
                class: "pharos-prefs-footer",
                if let Some(s) = status.as_ref() {
                    p { class: "pharos-prefs-status", "{s}" }
                }
                button {
                    class: "pharos-prefs-save",
                    onclick: move |_| on_action.call(PrefsAction::Save(draft.read().clone())),
                    "Save"
                }
            }
        }
    }
}

fn render_display(mut draft: Signal<UserConfiguration>) -> Element {
    let d = draft.read().clone();
    rsx! {
        form {
            class: "pharos-prefs-pane pharos-prefs-pane-display",
            onsubmit: move |ev| ev.prevent_default(),
            label {
                class: "pharos-prefs-row",
                input {
                    r#type: "checkbox",
                    checked: d.display_missing_episodes,
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.display_missing_episodes = ev.value() == "true" || ev.value() == "on";
                        draft.set(c);
                    },
                }
                "Show missing episodes"
            }
            label {
                class: "pharos-prefs-row",
                input {
                    r#type: "checkbox",
                    checked: d.display_collections_view,
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.display_collections_view = ev.value() == "true" || ev.value() == "on";
                        draft.set(c);
                    },
                }
                "Show Collections row in My Media"
            }
            label {
                class: "pharos-prefs-row",
                input {
                    r#type: "checkbox",
                    checked: d.hide_played_in_latest,
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.hide_played_in_latest = ev.value() == "true" || ev.value() == "on";
                        draft.set(c);
                    },
                }
                "Hide played items from Latest"
            }
        }
    }
}

fn render_playback(mut draft: Signal<UserConfiguration>) -> Element {
    let d = draft.read().clone();
    rsx! {
        form {
            class: "pharos-prefs-pane pharos-prefs-pane-playback",
            onsubmit: move |ev| ev.prevent_default(),
            label {
                class: "pharos-prefs-row",
                "Default audio language (ISO-639): "
                input {
                    r#type: "text",
                    value: "{d.audio_language_preference}",
                    autocomplete: "off",
                    oninput: move |ev| {
                        let mut c = draft.read().clone();
                        c.audio_language_preference = ev.value();
                        draft.set(c);
                    },
                }
            }
            label {
                class: "pharos-prefs-row",
                "Default subtitle language (ISO-639): "
                input {
                    r#type: "text",
                    value: "{d.subtitle_language_preference}",
                    autocomplete: "off",
                    oninput: move |ev| {
                        let mut c = draft.read().clone();
                        c.subtitle_language_preference = ev.value();
                        draft.set(c);
                    },
                }
            }
            label {
                class: "pharos-prefs-row",
                "Subtitle mode: "
                select {
                    value: "{d.subtitle_mode}",
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.subtitle_mode = ev.value();
                        draft.set(c);
                    },
                    {
                        ["Default", "Always", "OnlyForced", "None", "Smart"]
                            .iter()
                            .map(|m| {
                                let label = match *m {
                                    "OnlyForced" => "Only forced".to_string(),
                                    other => other.to_string(),
                                };
                                let value = m.to_string();
                                let selected = d.subtitle_mode == value;
                                rsx! {
                                    option {
                                        key: "{m}",
                                        value: "{value}",
                                        selected: selected,
                                        "{label}"
                                    }
                                }
                            })
                    }
                }
            }
            label {
                class: "pharos-prefs-row",
                input {
                    r#type: "checkbox",
                    checked: d.play_default_audio_track,
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.play_default_audio_track = ev.value() == "true" || ev.value() == "on";
                        draft.set(c);
                    },
                }
                "Play default audio track"
            }
            label {
                class: "pharos-prefs-row",
                input {
                    r#type: "checkbox",
                    checked: d.remember_audio_selections,
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.remember_audio_selections = ev.value() == "true" || ev.value() == "on";
                        draft.set(c);
                    },
                }
                "Remember audio track between sessions"
            }
            label {
                class: "pharos-prefs-row",
                input {
                    r#type: "checkbox",
                    checked: d.remember_subtitle_selections,
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.remember_subtitle_selections = ev.value() == "true" || ev.value() == "on";
                        draft.set(c);
                    },
                }
                "Remember subtitle track between sessions"
            }
        }
    }
}

fn render_languages(
    mut draft: Signal<UserConfiguration>,
    cultures: Vec<LocalizationCulture>,
) -> Element {
    let d = draft.read().clone();
    let audio_value = d.audio_language_preference.clone();
    let subtitle_value = d.subtitle_language_preference.clone();
    let cultures_for_audio = cultures.clone();
    let cultures_for_subtitle = cultures;
    rsx! {
        form {
            class: "pharos-prefs-pane pharos-prefs-pane-languages",
            onsubmit: move |ev| ev.prevent_default(),
            label {
                class: "pharos-prefs-row",
                "Preferred audio language: "
                select {
                    value: "{audio_value}",
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.audio_language_preference = ev.value();
                        draft.set(c);
                    },
                    option { value: "", "(any)" }
                    for c in cultures_for_audio.iter().cloned() {
                        option {
                            key: "{c.three_letter_iso}",
                            value: "{c.three_letter_iso}",
                            selected: c.three_letter_iso == audio_value,
                            "{c.name}"
                        }
                    }
                }
            }
            label {
                class: "pharos-prefs-row",
                "Preferred subtitle language: "
                select {
                    value: "{subtitle_value}",
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.subtitle_language_preference = ev.value();
                        draft.set(c);
                    },
                    option { value: "", "(any)" }
                    for c in cultures_for_subtitle.iter().cloned() {
                        option {
                            key: "{c.three_letter_iso}",
                            value: "{c.three_letter_iso}",
                            selected: c.three_letter_iso == subtitle_value,
                            "{c.name}"
                        }
                    }
                }
            }
        }
    }
}

fn render_home(mut draft: Signal<UserConfiguration>) -> Element {
    let d = draft.read().clone();
    rsx! {
        form {
            class: "pharos-prefs-pane pharos-prefs-pane-home",
            onsubmit: move |ev| ev.prevent_default(),
            label {
                class: "pharos-prefs-row",
                input {
                    r#type: "checkbox",
                    checked: d.enable_next_episode_auto_play,
                    onchange: move |ev| {
                        let mut c = draft.read().clone();
                        c.enable_next_episode_auto_play =
                            ev.value() == "true" || ev.value() == "on";
                        draft.set(c);
                    },
                }
                "Auto-play next episode"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn tabs_have_distinct_labels_and_keys() {
        let tabs = PrefsTab::all();
        assert_eq!(tabs.len(), 4);
        assert_eq!(tabs[0].label(), "Display");
        assert_eq!(tabs[1].class_suffix(), "playback");
        assert_eq!(tabs[2], PrefsTab::Home);
        assert_eq!(tabs[3].label(), "Languages");
        assert_eq!(tabs[3].class_suffix(), "languages");
    }

    #[test]
    fn action_value_semantics() {
        let a = PrefsAction::SelectTab(PrefsTab::Home);
        assert_eq!(a, a.clone());
        assert_ne!(a, PrefsAction::SelectTab(PrefsTab::Display));
    }
}
