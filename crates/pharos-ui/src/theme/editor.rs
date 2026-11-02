//! The theme editor's draft: a sparse theme document, edited one field at a
//! time, that previews through the catalogue and writes out as a theme file.

use super::doc::{
    ContrastDoc, EffectsDoc, KindDoc, MetaDoc, PaletteDoc, ShapeDoc, ThemeDoc, TypeDoc, SCHEMA,
};
use super::format;
use super::resolve::{Catalogue, Kind, PaletteRole, Resolved, ThemeError};
use super::value::{ContrastLevel, Density, FontStack};

/// The effect fields the editor sets with a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectField {
    Blur,
    Vignette,
    Grain,
    Glow,
}

/// A theme being edited. Values are stored as the user typed them; they are
/// checked when the draft is previewed or saved.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    doc: ThemeDoc,
}

impl Draft {
    /// A draft named `name` that extends `base` and overrides nothing yet.
    pub fn new(name: &str, base: &str) -> Self {
        Self {
            doc: ThemeDoc {
                meta: Some(MetaDoc {
                    name: Some(name.to_owned()),
                    schema: Some(SCHEMA),
                    extends: Some(base.to_owned()),
                    ..MetaDoc::default()
                }),
                ..ThemeDoc::default()
            },
        }
    }

    /// A draft holding the theme file `src`, for editing a custom theme.
    pub fn from_source(src: &str) -> Result<Self, ThemeError> {
        let (doc, _) = format::read(src)?;
        Ok(Self { doc })
    }

    /// `meta.name`, if the draft has one.
    pub fn name(&self) -> Option<&str> {
        self.doc.name()
    }

    /// Rename the draft.
    pub fn set_name(&mut self, name: &str) {
        self.meta().name = Some(name.to_owned());
    }

    /// Set the raw colour for a palette role.
    pub fn set_colour(&mut self, role: PaletteRole, hex: &str) {
        *self.palette().slot(role) = Some(hex.to_owned());
    }

    /// Set the raw colour for a library kind's accent.
    pub fn set_kind_colour(&mut self, kind: Kind, hex: &str) {
        let kinds = self.palette().kind.get_or_insert_with(KindDoc::default);
        *kinds.slot(kind) = Some(hex.to_owned());
    }

    /// Set the spacing density.
    pub fn set_density(&mut self, density: Density) {
        self.shape().density = Some(density.label().to_owned());
    }

    /// Set the interface font.
    pub fn set_ui_font(&mut self, font: FontStack) {
        self.type_().ui = Some(font.label().to_owned());
    }

    /// Set the display font.
    pub fn set_display_font(&mut self, font: FontStack) {
        self.type_().display = Some(font.label().to_owned());
    }

    /// Set one effect. Blur is a whole number of pixels, so it is rounded.
    pub fn set_effect(&mut self, field: EffectField, value: f64) {
        let effects = self.effects();
        match field {
            EffectField::Blur => effects.blur_px = Some(value.round() as i64),
            EffectField::Vignette => effects.vignette = Some(value),
            EffectField::Grain => effects.grain = Some(value),
            EffectField::Glow => effects.glow = Some(value),
        }
    }

    /// Set the WCAG level the theme is held to.
    pub fn set_level(&mut self, level: ContrastLevel) {
        self.contrast().level = Some(level.label().to_owned());
    }

    /// Add `role` to the decorative waivers, or remove it when already there.
    pub fn toggle_decorative(&mut self, role: PaletteRole) {
        let label = role.label();
        let waivers = self.contrast().decorative.get_or_insert_with(Vec::new);
        match waivers.iter().position(|r| r == label) {
            Some(i) => {
                waivers.remove(i);
            }
            None => waivers.push(label.to_owned()),
        }
    }

    /// The draft resolved over `catalogue`, without saving it there.
    pub fn preview(&self, catalogue: &Catalogue) -> Result<Resolved, ThemeError> {
        let name = self.doc.name().ok_or_else(|| ThemeError::MissingKey {
            path: "meta.name".to_owned(),
        })?;
        let mut next = catalogue.clone();
        next.insert(self.source()?)?;
        next.resolve(name)
    }

    /// The draft as a theme file.
    pub fn source(&self) -> Result<String, ThemeError> {
        format::write(&self.doc)
    }

    fn meta(&mut self) -> &mut MetaDoc {
        self.doc.meta.get_or_insert_with(MetaDoc::default)
    }

    fn palette(&mut self) -> &mut PaletteDoc {
        self.doc.palette.get_or_insert_with(PaletteDoc::default)
    }

    fn shape(&mut self) -> &mut ShapeDoc {
        self.doc.shape.get_or_insert_with(ShapeDoc::default)
    }

    fn type_(&mut self) -> &mut TypeDoc {
        self.doc.r#type.get_or_insert_with(TypeDoc::default)
    }

    fn effects(&mut self) -> &mut EffectsDoc {
        self.doc.effects.get_or_insert_with(EffectsDoc::default)
    }

    fn contrast(&mut self) -> &mut ContrastDoc {
        self.doc.contrast.get_or_insert_with(ContrastDoc::default)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::theme::resolve::Theme;
    use crate::theme::shipped;
    use crate::theme::value::Rgba;

    #[test]
    fn a_draft_with_only_the_accent_changed_previews_as_kali_dark_with_that_accent() {
        let catalogue = shipped();
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_colour(PaletteRole::Accent, "#ff00ff");

        let preview = draft.preview(&catalogue).unwrap();
        let mut expected: Theme = catalogue.resolve("Kali Dark").unwrap().theme;
        expected.name = "Mine".into();
        expected.palette.accent = Rgba::parse("#ff00ff").unwrap();
        assert_eq!(preview.theme, expected);
        assert!(preview.warnings.is_empty());
    }

    #[test]
    fn an_invalid_hex_previews_as_a_bad_value_naming_the_accent() {
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_colour(PaletteRole::Accent, "nope");
        let err = draft.preview(&shipped()).unwrap_err();
        assert!(
            matches!(&err, ThemeError::BadValue { path, value, .. }
                if path == "palette.accent" && value == "nope"),
            "{err:?}"
        );
    }

    #[test]
    fn source_round_trips_through_format_read_to_the_same_document() {
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_name("Mine 2");
        draft.set_colour(PaletteRole::Accent, "#ff00ff");
        draft.set_kind_colour(Kind::Movie, "#123456");
        draft.set_density(Density::Compact);
        draft.set_ui_font(FontStack::Mono);
        draft.set_display_font(FontStack::Serif);
        draft.set_effect(EffectField::Blur, 4.0);
        draft.set_effect(EffectField::Vignette, 0.25);
        draft.set_effect(EffectField::Grain, 0.5);
        draft.set_effect(EffectField::Glow, 0.75);
        draft.set_level(ContrastLevel::Aaa);
        draft.toggle_decorative(PaletteRole::Badge);

        let src = draft.source().unwrap();
        let (doc, warnings) = format::read(&src).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(doc, draft.doc);
        assert_eq!(Draft::from_source(&src).unwrap(), draft);
    }

    #[test]
    fn toggling_a_decorative_role_twice_removes_it() {
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.toggle_decorative(PaletteRole::Badge);
        draft.toggle_decorative(PaletteRole::Badge);
        assert_eq!(
            draft.doc.contrast.unwrap().decorative,
            Some(Vec::<String>::new())
        );
    }

    #[test]
    fn blur_is_stored_as_whole_pixels() {
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_effect(EffectField::Blur, 2.6);
        assert_eq!(draft.doc.effects.unwrap().blur_px, Some(3));
    }
}
