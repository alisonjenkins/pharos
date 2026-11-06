//! The Appearance page as a pure view: the theme gallery, the import forms
//! and the editor. Data in through props, events out through handlers; the
//! app maps them onto the theme state, so this renders under `dioxus-ssr`.

use dioxus::prelude::*;

use super::playback::{EffectsLevel, LibraryTransition, OverlayEffect};
use super::settings::Choices;
use crate::theme::slug;
use crate::theme::value::{
    Bounds, Density, FontStack, EFFECTS_BLUR_PX, EFFECTS_GLOW, EFFECTS_GRAIN, EFFECTS_VIGNETTE,
};

/// One gallery card: a theme, whether it ships with the app, whether it is
/// the active one, and its swatches as CSS colours (bg, surface, accent, fg).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeCard {
    pub name: String,
    pub shipped: bool,
    pub active: bool,
    pub swatches: Vec<String>,
}

/// A line of feedback under the page heading.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
}

/// One contrast pair checked by the editor.
#[derive(Debug, Clone, PartialEq)]
pub struct ContrastRow {
    pub pair: String,
    pub ratio: f64,
    pub min: f64,
    pub pass: bool,
    pub decorative: bool,
}

/// The editor for a draft theme. Colours are the hex as the user typed it.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorView {
    pub name: String,
    pub base: String,
    pub colours: Vec<(String, String)>,
    pub density: String,
    pub ui_font: String,
    pub effects: Vec<(String, f64)>,
    pub contrast: Vec<ContrastRow>,
    pub error: Option<String>,
}

/// One change the editor reports: a field label and its new value.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    Name(String),
    Colour(String, String),
    Density(String),
    UiFont(String),
    Effect(String, f64),
}

/// The slider bounds for an effect, by its label; the step is whole pixels
/// for blur and hundredths for the rest.
fn effect_range(label: &str) -> (Bounds, &'static str) {
    match label {
        "blur" => (EFFECTS_BLUR_PX, "1"),
        "vignette" => (EFFECTS_VIGNETTE, "0.01"),
        "grain" => (EFFECTS_GRAIN, "0.01"),
        _ => (EFFECTS_GLOW, "0.01"),
    }
}

#[component]
pub fn AppearanceView(
    themes: Vec<ThemeCard>,
    auto: bool,
    editor: Option<EditorView>,
    notice: Option<Notice>,
    #[props(default)] library_transition: LibraryTransition,
    #[props(default)] on_library_transition: EventHandler<LibraryTransition>,
    #[props(default)] overlay_effect: OverlayEffect,
    #[props(default)] on_overlay_effect: EventHandler<OverlayEffect>,
    #[props(default)] effects_level: EffectsLevel,
    #[props(default)] on_effects_level: EventHandler<EffectsLevel>,
    on_pick: EventHandler<String>,
    on_auto: EventHandler<()>,
    on_customise: EventHandler<String>,
    on_edit_custom: EventHandler<String>,
    on_delete: EventHandler<String>,
    on_export: EventHandler<String>,
    on_import_file: EventHandler<()>,
    on_import_paste: EventHandler<String>,
    on_edit: EventHandler<Edit>,
    on_save: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut paste = use_signal(String::new);
    let applied = themes
        .iter()
        .find(|card| card.active)
        .map(|card| card.name.clone());
    // Turning Follow the system off keeps the theme it resolved to, or the
    // first card when that one is not listed.
    let off_to = applied.or_else(|| themes.first().map(|card| card.name.clone()));
    let transitions = LibraryTransition::ALL
        .iter()
        .map(|fx| (fx.as_str().to_owned(), fx.label().to_owned()))
        .collect::<Vec<_>>();
    let effects = OverlayEffect::ALL
        .iter()
        .map(|fx| (fx.as_str().to_owned(), fx.label().to_owned()))
        .collect::<Vec<_>>();
    let levels = EffectsLevel::ALL
        .iter()
        .map(|level| (level.as_str().to_owned(), level.label().to_owned()))
        .collect::<Vec<_>>();
    rsx! {
        section { class: "pharos-settings-page pharos-appearance",
            h2 { "Appearance" }
            p { class: "pharos-settings-lede", "Themes and motion on this device." }
            if let Some(notice) = &notice {
                p {
                    class: "pharos-notice",
                    role: if notice.error { "alert" } else { "status" },
                    "{notice.text}"
                }
            }
            div { class: "pharos-settings-group",
                div { class: "pharos-setting",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "theme-auto-label", "Follow the system" }
                        p { class: "pharos-setting-help",
                            "Switch between your light and dark theme with the system's setting. Picking a theme below turns this off."
                        }
                    }
                    button {
                        r#type: "button",
                        id: "theme-auto",
                        class: "pharos-switch",
                        role: "switch",
                        aria_checked: if auto { "true" } else { "false" },
                        aria_labelledby: "theme-auto-label",
                        onclick: move |_| match (auto, off_to.clone()) {
                            (true, Some(name)) => on_pick.call(name),
                            (true, None) => {}
                            (false, _) => on_auto.call(()),
                        },
                    }
                }
                ul { class: "pharos-theme-gallery",
                    for card in themes.iter() {
                        {
                            let id = slug(&card.name);
                            let name = card.name.clone();
                            let colour = |i: usize| card.swatches.get(i).cloned().unwrap_or_default();
                            let preview = format!(
                                "--theme-bg: {}; --theme-surface: {}; --theme-accent: {}; --theme-fg: {}",
                                colour(0),
                                colour(1),
                                colour(2),
                                colour(3),
                            );
                            rsx! {
                                li {
                                    key: "{id}",
                                    class: "pharos-theme-card",
                                    "data-theme": "{id}",
                                    aria_current: card.active.then_some("true"),
                                    button {
                                        r#type: "button",
                                        class: "pharos-theme-preview",
                                        id: "theme-apply-{id}",
                                        aria_label: "Apply {card.name}",
                                        style: "{preview}",
                                        onclick: {
                                            let name = name.clone();
                                            move |_| on_pick.call(name.clone())
                                        },
                                        span { class: "pharos-theme-mock", "aria-hidden": "true",
                                            span { class: "pharos-theme-mock-bar" }
                                            span { class: "pharos-theme-mock-tiles",
                                                span {}
                                                span {}
                                                span {}
                                            }
                                            span { class: "pharos-theme-mock-button" }
                                        }
                                    }
                                    div { class: "pharos-theme-label",
                                        p { class: "pharos-theme-name", "{card.name}" }
                                        if card.active {
                                            span { class: "pharos-theme-in-use", "In use" }
                                        }
                                        div { class: "pharos-swatches",
                                            for colour in card.swatches.iter() {
                                                span {
                                                    class: "pharos-swatch",
                                                    style: "background: {colour}",
                                                    aria_hidden: "true",
                                                }
                                            }
                                        }
                                    }
                                    div { class: "pharos-theme-actions",
                                        button {
                                            r#type: "button",
                                            id: "theme-customise-{id}",
                                            onclick: {
                                                let name = name.clone();
                                                move |_| on_customise.call(name.clone())
                                            },
                                            "Customise"
                                        }
                                        if !card.shipped {
                                            button {
                                                r#type: "button",
                                                id: "theme-edit-{id}",
                                                onclick: {
                                                    let name = name.clone();
                                                    move |_| on_edit_custom.call(name.clone())
                                                },
                                                "Edit"
                                            }
                                            button {
                                                r#type: "button",
                                                id: "theme-delete-{id}",
                                                onclick: {
                                                    let name = name.clone();
                                                    move |_| on_delete.call(name.clone())
                                                },
                                                "Delete"
                                            }
                                        }
                                        button {
                                            r#type: "button",
                                            id: "theme-export-{id}",
                                            onclick: {
                                                let name = name.clone();
                                                move |_| on_export.call(name.clone())
                                            },
                                            "Export"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(editor) = editor {
                {
                    let blocked = editor.contrast.iter().any(|row| !row.decorative && !row.pass);
                    rsx! {
                        form {
                            class: "pharos-theme-editor",
                            id: "theme-editor",
                            onsubmit: move |evt: FormEvent| evt.prevent_default(),
                            div { class: "pharos-editor-fields",
                                p { class: "pharos-muted", "Based on {editor.base}" }
                                label { r#for: "theme-name", "Name" }
                                input {
                                    id: "theme-name",
                                    r#type: "text",
                                    value: "{editor.name}",
                                    oninput: move |e| on_edit.call(Edit::Name(e.value())),
                                }
                                fieldset {
                                    legend { "Colours" }
                                    for (role, hex) in editor.colours.iter() {
                                        {
                                            let id = slug(role);
                                            let key = role.clone();
                                            rsx! {
                                                div { class: "pharos-colour",
                                                    label { r#for: "theme-colour-{id}", "{role}" }
                                                    input {
                                                        r#type: "color",
                                                        id: "theme-colour-{id}",
                                                        value: "{hex}",
                                                        oninput: move |e| on_edit.call(Edit::Colour(key.clone(), e.value())),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                label { r#for: "theme-density", "Density" }
                                select {
                                    id: "theme-density",
                                    onchange: move |e: FormEvent| on_edit.call(Edit::Density(e.value())),
                                    for density in Density::ALL.iter() {
                                        option {
                                            value: "{density.label()}",
                                            selected: density.label() == editor.density,
                                            "{density.label()}"
                                        }
                                    }
                                }
                                label { r#for: "theme-ui-font", "UI font" }
                                select {
                                    id: "theme-ui-font",
                                    onchange: move |e: FormEvent| on_edit.call(Edit::UiFont(e.value())),
                                    for font in FontStack::ALL.iter() {
                                        option {
                                            value: "{font.label()}",
                                            selected: font.label() == editor.ui_font,
                                            "{font.label()}"
                                        }
                                    }
                                }
                                for (label, value) in editor.effects.iter() {
                                    {
                                        let id = slug(label);
                                        let key = label.clone();
                                        let (bounds, step) = effect_range(label);
                                        rsx! {
                                            div { class: "pharos-effect",
                                                label { r#for: "theme-effect-{id}", "{label}" }
                                                input {
                                                    r#type: "range",
                                                    id: "theme-effect-{id}",
                                                    min: "{bounds.min}",
                                                    max: "{bounds.max}",
                                                    step: "{step}",
                                                    value: "{value}",
                                                    oninput: move |e| {
                                                        if let Ok(v) = e.value().parse::<f64>() {
                                                            on_edit.call(Edit::Effect(key.clone(), v));
                                                        }
                                                    },
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "pharos-editor-check",
                                table { class: "pharos-contrast",
                                    caption { "Contrast" }
                                    thead {
                                        tr {
                                            th { scope: "col", "Pair" }
                                            th { scope: "col", "Ratio" }
                                            th { scope: "col", "Minimum" }
                                            th { scope: "col", "Result" }
                                        }
                                    }
                                    tbody {
                                        for row in editor.contrast.iter() {
                                            tr {
                                                th { scope: "row", "{row.pair}" }
                                                td { "{row.ratio:.2}:1" }
                                                td { "{row.min:.1}:1" }
                                                td {
                                                    if row.pass { "Pass" } else { "Fail" }
                                                    if row.decorative { " (decorative)" }
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some(error) = &editor.error {
                                    p { class: "pharos-error", role: "alert", "{error}" }
                                }
                                div { class: "pharos-editor-actions",
                                    button {
                                        r#type: "button",
                                        id: "theme-save",
                                        disabled: blocked,
                                        onclick: move |_| on_save.call(()),
                                        "Save"
                                    }
                                    button {
                                        r#type: "button",
                                        id: "theme-cancel",
                                        onclick: move |_| on_cancel.call(()),
                                        "Cancel"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "pharos-settings-group",
                div { class: "pharos-setting pharos-library-transition",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "library-transition-label", "Library transition" }
                        p { id: "library-transition-help", class: "pharos-setting-help",
                            "How cards arrive when you open or switch libraries. Saved on this device. Off whenever the system asks for reduced motion."
                        }
                    }
                    Choices {
                        id: "library-transition",
                        labelled_by: "library-transition-label",
                        described_by: "library-transition-help".to_owned(),
                        options: transitions,
                        selected: library_transition.as_str().to_owned(),
                        on_pick: move |value: String| on_library_transition.call(LibraryTransition::parse(&value)),
                    }
                }
                div { class: "pharos-setting pharos-overlay-effect",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "overlay-effect-label", "Controls effect" }
                        p { id: "overlay-effect-help", class: "pharos-setting-help",
                            "How the player's controls hide and return during playback. Saved on this device. A plain fade whenever the system asks for reduced motion."
                        }
                    }
                    Choices {
                        id: "overlay-effect",
                        labelled_by: "overlay-effect-label",
                        described_by: "overlay-effect-help".to_owned(),
                        options: effects,
                        selected: overlay_effect.as_str().to_owned(),
                        on_pick: move |value: String| on_overlay_effect.call(OverlayEffect::parse(&value)),
                    }
                }
                div { class: "pharos-setting pharos-effects-level",
                    div { class: "pharos-setting-text",
                        p { class: "pharos-setting-name", id: "effects-level-label", "Effects" }
                        p { id: "effects-level-help", class: "pharos-setting-help",
                            "How much motion styled subtitles carry. Off keeps the plain subtitles. Saved on this device. Static whenever the system asks for reduced motion."
                        }
                    }
                    Choices {
                        id: "effects-level",
                        labelled_by: "effects-level-label",
                        described_by: "effects-level-help".to_owned(),
                        options: levels,
                        selected: effects_level.as_str().to_owned(),
                        on_pick: move |value: String| on_effects_level.call(EffectsLevel::parse(&value)),
                    }
                }
            }
            div { class: "pharos-settings-group pharos-import",
                div { class: "pharos-setting",
                    div { class: "pharos-setting-text",
                        h3 { class: "pharos-setting-name", "Import a theme" }
                        p { class: "pharos-setting-help",
                            "A theme file exported from Pharos on another device, or pasted below."
                        }
                    }
                    button {
                        r#type: "button",
                        id: "theme-import-file",
                        onclick: move |_| on_import_file.call(()),
                        "Choose a theme file"
                    }
                }
                div { class: "pharos-setting pharos-import-paste",
                    label { class: "pharos-setting-name", r#for: "theme-import-paste", "Paste a theme file" }
                    textarea {
                        id: "theme-import-paste",
                        rows: "4",
                        value: "{paste}",
                        oninput: move |e| paste.set(e.value()),
                    }
                    button {
                        r#type: "button",
                        id: "theme-import-paste-go",
                        onclick: move |_| on_import_paste.call(paste.read().clone()),
                        "Import pasted theme"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(body: impl Fn() -> Element + 'static) -> String {
        let body: std::rc::Rc<dyn Fn() -> Element> = std::rc::Rc::new(body);
        let mut dom =
            VirtualDom::new_with_props(|body: std::rc::Rc<dyn Fn() -> Element>| body(), body);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn page(themes: Vec<ThemeCard>, editor: Option<EditorView>, notice: Option<Notice>) -> String {
        page_auto(themes, editor, notice, false)
    }

    fn page_auto(
        themes: Vec<ThemeCard>,
        editor: Option<EditorView>,
        notice: Option<Notice>,
        auto: bool,
    ) -> String {
        render(move || {
            rsx! {
                AppearanceView {
                    themes: themes.clone(),
                    auto,
                    editor: editor.clone(),
                    notice: notice.clone(),
                    on_pick: |_| {},
                    on_auto: |_| {},
                    on_customise: |_| {},
                    on_edit_custom: |_| {},
                    on_delete: |_| {},
                    on_export: |_| {},
                    on_import_file: |_| {},
                    on_import_paste: |_| {},
                    on_edit: |_| {},
                    on_save: |_| {},
                    on_cancel: |_| {},
                }
            }
        })
    }

    fn page_with_library_transition(fx: Option<LibraryTransition>) -> String {
        render(move || {
            rsx! {
                AppearanceView {
                    themes: Vec::new(),
                    auto: false,
                    editor: None,
                    notice: None,
                    library_transition: fx.unwrap_or_default(),
                    on_library_transition: |_| {},
                    on_pick: |_| {},
                    on_auto: |_| {},
                    on_customise: |_| {},
                    on_edit_custom: |_| {},
                    on_delete: |_| {},
                    on_export: |_| {},
                    on_import_file: |_| {},
                    on_import_paste: |_| {},
                    on_edit: |_| {},
                    on_save: |_| {},
                    on_cancel: |_| {},
                }
            }
        })
    }

    fn library_transition_group(html: &str) -> &str {
        let start = html.find("id=\"library-transition\"").unwrap_or(html.len());
        let end = html[start..]
            .find("</div>")
            .map_or(html.len(), |e| start + e);
        &html[start..end]
    }

    #[test]
    fn library_transition_presses_only_the_stored_choice() {
        let html = page_with_library_transition(Some(LibraryTransition::Matrix));
        let group = library_transition_group(&html);
        assert!(!group.is_empty(), "choices missing: {html}");
        assert_eq!(group.matches("aria-pressed=\"true\"").count(), 1, "{group}");
        assert!(
            start_tag(group, "id=\"library-transition-matrix\"").contains("aria-pressed=\"true\""),
            "{group}"
        );
        for fx in LibraryTransition::ALL {
            assert!(
                group.contains(fx.label()),
                "{} missing: {group}",
                fx.label()
            );
        }
        assert!(html.contains("reduced motion"), "{html}");
    }

    #[test]
    fn library_transition_defaults_to_fade() {
        let html = page_with_library_transition(None);
        let group = library_transition_group(&html);
        assert_eq!(group.matches("aria-pressed=\"true\"").count(), 1, "{group}");
        assert!(
            start_tag(group, "id=\"library-transition-fade\"").contains("aria-pressed=\"true\""),
            "{group}"
        );
    }

    fn page_with_overlay_effect(fx: Option<OverlayEffect>) -> String {
        render(move || {
            rsx! {
                AppearanceView {
                    themes: Vec::new(),
                    auto: false,
                    editor: None,
                    notice: None,
                    overlay_effect: fx.unwrap_or_default(),
                    on_overlay_effect: |_| {},
                    on_pick: |_| {},
                    on_auto: |_| {},
                    on_customise: |_| {},
                    on_edit_custom: |_| {},
                    on_delete: |_| {},
                    on_export: |_| {},
                    on_import_file: |_| {},
                    on_import_paste: |_| {},
                    on_edit: |_| {},
                    on_save: |_| {},
                    on_cancel: |_| {},
                }
            }
        })
    }

    fn overlay_effect_group(html: &str) -> &str {
        let start = html.find("id=\"overlay-effect\"").unwrap_or(html.len());
        let end = html[start..]
            .find("</div>")
            .map_or(html.len(), |e| start + e);
        &html[start..end]
    }

    #[test]
    fn overlay_effect_presses_only_the_stored_choice() {
        let html = page_with_overlay_effect(Some(OverlayEffect::Burn));
        let group = overlay_effect_group(&html);
        assert!(!group.is_empty(), "choices missing: {html}");
        assert_eq!(group.matches("aria-pressed=\"true\"").count(), 1, "{group}");
        assert!(
            start_tag(group, "id=\"overlay-effect-burn\"").contains("aria-pressed=\"true\""),
            "{group}"
        );
        for fx in OverlayEffect::ALL {
            assert!(
                group.contains(fx.label()),
                "{} missing: {group}",
                fx.label()
            );
        }
        assert!(html.contains("reduced motion"), "{html}");
    }

    #[test]
    fn overlay_effect_defaults_to_fade() {
        let html = page_with_overlay_effect(None);
        let group = overlay_effect_group(&html);
        assert_eq!(group.matches("aria-pressed=\"true\"").count(), 1, "{group}");
        assert!(
            start_tag(group, "id=\"overlay-effect-fade\"").contains("aria-pressed=\"true\""),
            "{group}"
        );
    }

    fn page_with_effects_level(level: Option<EffectsLevel>) -> String {
        render(move || {
            rsx! {
                AppearanceView {
                    themes: Vec::new(),
                    auto: false,
                    editor: None,
                    notice: None,
                    effects_level: level.unwrap_or_default(),
                    on_effects_level: |_| {},
                    on_pick: |_| {},
                    on_auto: |_| {},
                    on_customise: |_| {},
                    on_edit_custom: |_| {},
                    on_delete: |_| {},
                    on_export: |_| {},
                    on_import_file: |_| {},
                    on_import_paste: |_| {},
                    on_edit: |_| {},
                    on_save: |_| {},
                    on_cancel: |_| {},
                }
            }
        })
    }

    fn effects_level_group(html: &str) -> &str {
        let start = html.find("id=\"effects-level\"").unwrap_or(html.len());
        let end = html[start..]
            .find("</div>")
            .map_or(html.len(), |e| start + e);
        &html[start..end]
    }

    #[test]
    fn effects_level_presses_only_the_stored_choice() {
        let html = page_with_effects_level(Some(EffectsLevel::Full));
        let group = effects_level_group(&html);
        assert!(!group.is_empty(), "choices missing: {html}");
        assert_eq!(group.matches("aria-pressed=\"true\"").count(), 1, "{group}");
        assert!(
            start_tag(group, "id=\"effects-level-full\"").contains("aria-pressed=\"true\""),
            "{group}"
        );
        for level in EffectsLevel::ALL {
            assert!(
                group.contains(level.label()),
                "{} missing: {group}",
                level.label()
            );
        }
        assert!(html.contains("Effects"), "{html}");
    }

    #[test]
    fn effects_level_defaults_to_subtle() {
        let html = page_with_effects_level(None);
        let group = effects_level_group(&html);
        assert_eq!(group.matches("aria-pressed=\"true\"").count(), 1, "{group}");
        assert!(
            start_tag(group, "id=\"effects-level-subtle\"").contains("aria-pressed=\"true\""),
            "{group}"
        );
    }

    #[test]
    fn follow_the_system_is_a_switch_showing_auto() {
        let html = page(seven_cards(), None, None);
        let switch = start_tag(&html, "id=\"theme-auto\"");
        assert!(html.contains("role=\"switch\""), "{html}");
        assert!(switch.contains("aria-checked=\"false\""), "{switch}");
        let on = page_auto(seven_cards(), None, None, true);
        let switch = start_tag(&on, "id=\"theme-auto\"");
        assert!(switch.contains("aria-checked=\"true\""), "{switch}");
    }

    #[test]
    fn each_card_previews_its_own_colours() {
        let html = page(vec![card("Pharos Night", true, true)], None, None);
        assert!(
            html.contains("--theme-bg: #0e1116; --theme-surface: #1a2030; --theme-accent: #4cb4ff; --theme-fg: #e6e8eb"),
            "{html}"
        );
        assert!(html.contains("aria-label=\"Apply Pharos Night\""), "{html}");
        assert!(html.contains("In use"), "{html}");
    }

    fn card(name: &str, shipped: bool, active: bool) -> ThemeCard {
        ThemeCard {
            name: name.to_owned(),
            shipped,
            active,
            swatches: vec![
                "#0e1116".into(),
                "#1a2030".into(),
                "#4cb4ff".into(),
                "#e6e8eb".into(),
            ],
        }
    }

    fn seven_cards() -> Vec<ThemeCard> {
        [
            "Pharos Night",
            "Marquee",
            "Kali Dark",
            "Kali Purple",
            "Neon Grid",
            "Daylight",
            "High Contrast",
        ]
        .iter()
        .map(|name| card(name, true, *name == "Kali Dark"))
        .collect()
    }

    fn editor(contrast: Vec<ContrastRow>) -> EditorView {
        EditorView {
            name: "Mine".into(),
            base: "Kali Dark".into(),
            colours: vec![("accent".into(), "#4cb4ff".into())],
            density: "comfortable".into(),
            ui_font: "system-ui".into(),
            effects: vec![("blur".into(), 4.0), ("glow".into(), 0.5)],
            contrast,
            error: None,
        }
    }

    fn row(pass: bool, decorative: bool) -> ContrastRow {
        ContrastRow {
            pair: "fg on bg".into(),
            ratio: 4.2,
            min: 4.5,
            pass,
            decorative,
        }
    }

    /// The start tag of the element whose opening tag contains `marker`.
    fn start_tag<'a>(html: &'a str, marker: &str) -> &'a str {
        let at = html.find(marker).unwrap_or(0);
        let rest = html.get(at..).unwrap_or("");
        let end = rest.find('>').unwrap_or(rest.len());
        rest.get(..end).unwrap_or("")
    }

    #[test]
    fn the_gallery_marks_only_the_active_card() {
        let html = page(seven_cards(), None, None);
        assert_eq!(
            html.matches("class=\"pharos-theme-card\"").count(),
            7,
            "{html}"
        );
        assert_eq!(html.matches("aria-current=\"true\"").count(), 1, "{html}");
        let kali = html
            .split("<li")
            .find(|chunk| chunk.contains("data-theme=\"kali-dark\""))
            .unwrap_or("");
        assert!(kali.contains("aria-current=\"true\""), "{html}");
        assert!(html.contains("style=\"background: #4cb4ff\""), "{html}");
    }

    #[test]
    fn save_is_disabled_when_a_non_decorative_pair_fails() {
        let html = page(
            vec![],
            Some(editor(vec![row(true, false), row(false, false)])),
            None,
        );
        assert!(
            start_tag(&html, "id=\"theme-save\"").contains("disabled"),
            "{html}"
        );
        assert!(html.contains("Fail"), "{html}");
    }

    #[test]
    fn a_failing_decorative_pair_does_not_block_save() {
        let html = page(vec![], Some(editor(vec![row(false, true)])), None);
        assert!(
            !start_tag(&html, "id=\"theme-save\"").contains("disabled"),
            "{html}"
        );
        assert!(html.contains("(decorative)"), "{html}");
    }

    #[test]
    fn an_editor_error_is_an_alert() {
        let mut ed = editor(vec![row(true, false)]);
        ed.error = Some("palette.accent is not a colour".into());
        let html = page(vec![], Some(ed), None);
        assert!(
            html.contains("role=\"alert\"") && html.contains("palette.accent is not a colour"),
            "{html}"
        );
    }

    #[test]
    fn a_notice_is_a_status_unless_it_is_an_error() {
        let ok = page(
            vec![],
            None,
            Some(Notice {
                text: "Imported Mine".into(),
                error: false,
            }),
        );
        assert!(
            ok.contains("role=\"status\"") && ok.contains("Imported Mine"),
            "{ok}"
        );
        let bad = page(
            vec![],
            None,
            Some(Notice {
                text: "Refused".into(),
                error: true,
            }),
        );
        assert!(
            bad.contains("role=\"alert\"") && !bad.contains("role=\"status\""),
            "{bad}"
        );
    }

    #[test]
    fn only_custom_cards_offer_delete() {
        let html = page(
            vec![card("Marquee", true, false), card("My Theme", false, false)],
            None,
            None,
        );
        assert!(html.contains("id=\"theme-delete-my-theme\""), "{html}");
        assert!(!html.contains("id=\"theme-delete-marquee\""), "{html}");
        assert!(html.contains("id=\"theme-export-marquee\""), "{html}");
    }
}
