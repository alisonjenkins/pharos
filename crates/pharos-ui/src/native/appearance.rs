//! The Appearance page as a pure view: the theme gallery, the import forms
//! and the editor. Data in through props, events out through handlers; the
//! app maps them onto the theme state, so this renders under `dioxus-ssr`.

use dioxus::prelude::*;

use super::playback::{LibraryTransition, SubtitleSize, MAX_BITRATE_CHOICES};
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
    diagnostics: bool,
    diagnostics_key: u32,
    on_diagnostics: EventHandler<bool>,
    max_quality: u64,
    on_max_quality: EventHandler<u64>,
    subtitle_size: SubtitleSize,
    on_subtitle_size: EventHandler<SubtitleSize>,
    #[props(default)] library_transition: LibraryTransition,
    #[props(default)] on_library_transition: EventHandler<LibraryTransition>,
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
    rsx! {
        section { class: "pharos-appearance",
            h2 { "Appearance" }
            if let Some(notice) = &notice {
                p {
                    class: "pharos-notice",
                    role: if notice.error { "alert" } else { "status" },
                    "{notice.text}"
                }
            }
            button {
                r#type: "button",
                id: "theme-auto",
                aria_pressed: if auto { "true" } else { "false" },
                onclick: move |_| on_auto.call(()),
                "Auto (follow the system)"
            }
            fieldset { class: "pharos-max-quality",
                legend { "Maximum quality" }
                label { r#for: "max-quality", "Limit video bitrate on this device" }
                select {
                    id: "max-quality",
                    aria_describedby: "max-quality-help",
                    onchange: move |ev| {
                        if let Ok(bps) = ev.value().parse::<u64>() {
                            on_max_quality.call(bps);
                        }
                    },
                    for (bps , label) in MAX_BITRATE_CHOICES {
                        option {
                            key: "{bps}",
                            value: "{bps}",
                            selected: bps == max_quality,
                            "{label}"
                        }
                    }
                }
                p { id: "max-quality-help",
                    "Auto adapts to your connection: it lowers the video quality when playback keeps stalling and raises it again when the link allows. A limit you pick is a ceiling Auto never goes above."
                }
            }
            fieldset { class: "pharos-subtitle-size",
                legend { "Subtitle size" }
                label { r#for: "subtitle-size", "Text size of subtitles on this device" }
                select {
                    id: "subtitle-size",
                    aria_describedby: "subtitle-size-help",
                    onchange: move |ev| {
                        on_subtitle_size.call(SubtitleSize::parse(&ev.value()));
                    },
                    for size in SubtitleSize::ALL {
                        option {
                            key: "{size.as_str()}",
                            value: "{size.as_str()}",
                            selected: size == subtitle_size,
                            "{size.label()}"
                        }
                    }
                }
                p { id: "subtitle-size-help",
                    "Saved on this device."
                }
            }
            fieldset { class: "pharos-library-transition",
                legend { "Library transition" }
                label { r#for: "library-transition", "How cards arrive when you open or switch libraries" }
                select {
                    id: "library-transition",
                    aria_describedby: "library-transition-help",
                    onchange: move |ev| {
                        on_library_transition.call(LibraryTransition::parse(&ev.value()));
                    },
                    for fx in LibraryTransition::ALL {
                        option {
                            key: "{fx.as_str()}",
                            value: "{fx.as_str()}",
                            selected: fx == library_transition,
                            "{fx.label()}"
                        }
                    }
                }
                p { id: "library-transition-help",
                    "Saved on this device. Off whenever the system asks for reduced motion."
                }
            }
            fieldset { class: "pharos-diagnostics",
                legend { "Diagnostics" }
                label {
                    // Keyed so a failed save rebuilds the box: the DOM keeps the
                    // user's flip, and an unchanged `checked` never resets it.
                    input {
                        key: "{diagnostics_key}",
                        r#type: "checkbox",
                        id: "diagnostics-opt-in",
                        checked: diagnostics,
                        aria_describedby: "diagnostics-help",
                        onchange: move |ev| on_diagnostics.call(ev.checked()),
                    }
                    "Send diagnostics to this server"
                }
                p { id: "diagnostics-help",
                    "Off by default, and saved to your account on this server. When on, playback errors and theme fallbacks are sent to this server's logs. Stream URLs and credentials are never sent."
                }
            }
            ul { class: "pharos-theme-gallery",
                for card in themes.iter() {
                    {
                        let id = slug(&card.name);
                        let name = card.name.clone();
                        rsx! {
                            li {
                                key: "{id}",
                                class: "pharos-theme-card",
                                "data-theme": "{id}",
                                aria_current: card.active.then_some("true"),
                                div { class: "pharos-swatches",
                                    for colour in card.swatches.iter() {
                                        span {
                                            class: "pharos-swatch",
                                            style: "background: {colour}",
                                            aria_hidden: "true",
                                        }
                                    }
                                }
                                p { class: "pharos-theme-name", "{card.name}" }
                                div { class: "pharos-theme-actions",
                                    button {
                                        r#type: "button",
                                        id: "theme-apply-{id}",
                                        onclick: {
                                            let name = name.clone();
                                            move |_| on_pick.call(name.clone())
                                        },
                                        "Apply"
                                    }
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
            div { class: "pharos-import",
                h3 { "Import a theme" }
                button {
                    r#type: "button",
                    id: "theme-import-file",
                    onclick: move |_| on_import_file.call(()),
                    "Choose a theme file"
                }
                label { r#for: "theme-import-paste", "Paste a theme file" }
                textarea {
                    id: "theme-import-paste",
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
        page_with_diagnostics(themes, editor, notice, false)
    }

    fn page_with_diagnostics(
        themes: Vec<ThemeCard>,
        editor: Option<EditorView>,
        notice: Option<Notice>,
        diagnostics: bool,
    ) -> String {
        page_full(themes, editor, notice, diagnostics, 0)
    }

    fn page_full(
        themes: Vec<ThemeCard>,
        editor: Option<EditorView>,
        notice: Option<Notice>,
        diagnostics: bool,
        max_quality: u64,
    ) -> String {
        page_with_subtitle_size(
            themes,
            editor,
            notice,
            diagnostics,
            max_quality,
            SubtitleSize::default(),
        )
    }

    fn page_with_subtitle_size(
        themes: Vec<ThemeCard>,
        editor: Option<EditorView>,
        notice: Option<Notice>,
        diagnostics: bool,
        max_quality: u64,
        subtitle_size: SubtitleSize,
    ) -> String {
        render(move || {
            rsx! {
                AppearanceView {
                    themes: themes.clone(),
                    auto: false,
                    editor: editor.clone(),
                    notice: notice.clone(),
                    diagnostics,
                    diagnostics_key: 0,
                    max_quality,
                    on_max_quality: |_| {},
                    subtitle_size,
                    on_subtitle_size: |_| {},
                    on_diagnostics: |_| {},
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

    fn diagnostics_box(html: &str) -> &str {
        html.find("id=\"diagnostics-opt-in\"")
            .and_then(|at| {
                let start = html[..at].rfind('<')?;
                let end = at + html[at..].find('>')?;
                html.get(start..=end)
            })
            .unwrap_or("<toggle not rendered>")
    }

    #[test]
    fn diagnostics_box_renders_the_server_value_it_is_given() {
        let on = page_with_diagnostics(Vec::new(), None, None, true);
        assert!(
            diagnostics_box(&on).contains("checked"),
            "a server value of on must render checked: {}",
            diagnostics_box(&on)
        );
        let off = page_with_diagnostics(Vec::new(), None, None, false);
        assert!(
            !diagnostics_box(&off).contains("checked"),
            "a server value of off must render unchecked: {}",
            diagnostics_box(&off)
        );
    }

    #[test]
    fn max_quality_select_marks_only_the_stored_choice() {
        let html = page_full(Vec::new(), None, None, false, 2_000_000);
        let start = html.find("id=\"max-quality\"").unwrap_or(usize::MAX);
        assert!(start != usize::MAX, "select missing: {html}");
        let sel = &html[start
            ..html[start..]
                .find("</select>")
                .map_or(html.len(), |e| start + e)];
        assert_eq!(sel.matches("selected").count(), 1, "{sel}");
        assert!(
            sel.contains("value=\"2000000\" selected") || sel.contains("selected=\"true\""),
            "{sel}"
        );
        for (_, label) in MAX_BITRATE_CHOICES {
            assert!(sel.contains(label), "{label} missing: {sel}");
        }
    }

    #[test]
    fn max_quality_defaults_to_auto_and_says_auto_adapts() {
        let html = page_full(Vec::new(), None, None, false, 0);
        let start = html.find("id=\"max-quality\"").unwrap_or(usize::MAX);
        assert!(start != usize::MAX, "select missing: {html}");
        let sel = &html[start
            ..html[start..]
                .find("</select>")
                .map_or(html.len(), |e| start + e)];
        assert_eq!(sel.matches("selected").count(), 1, "{sel}");
        assert!(sel.contains("value=\"0\" selected") || sel.contains("selected=\"true\""));
        assert!(html.contains("adapts to your connection"), "{html}");
        assert!(html.contains("never goes above"), "{html}");
    }

    #[test]
    fn subtitle_size_select_marks_only_the_stored_choice() {
        let html = page_with_subtitle_size(Vec::new(), None, None, false, 0, SubtitleSize::Large);
        let start = html.find("id=\"subtitle-size\"").unwrap_or(usize::MAX);
        assert!(start != usize::MAX, "select missing: {html}");
        let sel = &html[start
            ..html[start..]
                .find("</select>")
                .map_or(html.len(), |e| start + e)];
        assert_eq!(sel.matches("selected").count(), 1, "{sel}");
        assert!(
            sel.contains("value=\"large\" selected") || sel.contains("selected=\"true\""),
            "{sel}"
        );
        for size in SubtitleSize::ALL {
            assert!(
                sel.contains(size.label()),
                "{} missing: {sel}",
                size.label()
            );
        }
    }

    #[test]
    fn subtitle_size_defaults_to_medium() {
        let html = page_full(Vec::new(), None, None, false, 0);
        let start = html.find("id=\"subtitle-size\"").unwrap_or(usize::MAX);
        assert!(start != usize::MAX, "select missing: {html}");
        let sel = &html[start
            ..html[start..]
                .find("</select>")
                .map_or(html.len(), |e| start + e)];
        assert_eq!(sel.matches("selected").count(), 1, "{sel}");
        assert!(
            sel.contains("value=\"medium\" selected") || sel.contains("selected=\"true\""),
            "{sel}"
        );
        assert!(html.contains("Saved on this device."), "{html}");
    }

    fn page_with_library_transition(fx: Option<LibraryTransition>) -> String {
        render(move || {
            rsx! {
                AppearanceView {
                    themes: Vec::new(),
                    auto: false,
                    editor: None,
                    notice: None,
                    diagnostics: false,
                    diagnostics_key: 0,
                    max_quality: 0,
                    on_max_quality: |_| {},
                    subtitle_size: SubtitleSize::default(),
                    on_subtitle_size: |_| {},
                    library_transition: fx.unwrap_or_default(),
                    on_library_transition: |_| {},
                    on_diagnostics: |_| {},
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

    fn library_transition_select(html: &str) -> &str {
        let start = html.find("id=\"library-transition\"").unwrap_or(html.len());
        let end = html[start..]
            .find("</select>")
            .map_or(html.len(), |e| start + e);
        &html[start..end]
    }

    #[test]
    fn library_transition_select_marks_only_the_stored_choice() {
        let html = page_with_library_transition(Some(LibraryTransition::Matrix));
        let sel = library_transition_select(&html);
        assert!(!sel.is_empty(), "select missing: {html}");
        assert_eq!(sel.matches("selected").count(), 1, "{sel}");
        assert!(
            sel.contains("value=\"matrix\" selected") || sel.contains("selected=\"true\""),
            "{sel}"
        );
        for fx in LibraryTransition::ALL {
            assert!(sel.contains(fx.label()), "{} missing: {sel}", fx.label());
        }
        assert!(html.contains("reduced motion"), "{html}");
    }

    #[test]
    fn library_transition_defaults_to_fade() {
        let sel_html = page_with_library_transition(None);
        let sel = library_transition_select(&sel_html);
        assert_eq!(sel.matches("selected").count(), 1, "{sel}");
        assert!(
            sel.contains("value=\"fade\" selected") || sel.contains("selected=\"true\""),
            "{sel}"
        );
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
