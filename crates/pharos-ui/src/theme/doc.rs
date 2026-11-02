//! The theme file as written: raw strings and numbers, one `Option` per key.
//! Reading and writing the file is `format`'s job; what the values mean is
//! checked when a theme is resolved.

use serde::{Deserialize, Serialize};

use super::resolve::{Kind, PaletteRole};

/// The largest theme file accepted, in bytes.
pub const MAX_BYTES: usize = 64 * 1024;
/// The newest `meta.schema` this build understands.
pub const SCHEMA: u32 = 1;
pub(super) const NAME_MAX_CHARS: usize = 64;
const CLIP_CHARS: usize = 64;

/// Layering one document over another: a value present in `over` replaces the
/// one in `self`, and a section present in both is layered field by field.
trait Overlay {
    fn overlay(&mut self, over: &Self);
}

impl<T: Overlay + Clone> Overlay for Option<T> {
    fn overlay(&mut self, over: &Self) {
        match (self.as_mut(), over) {
            (Some(base), Some(over)) => base.overlay(over),
            (None, Some(over)) => *self = Some(over.clone()),
            (_, None) => {}
        }
    }
}

macro_rules! replaced_whole {
    ($($ty:ty),+) => {
        $(
            impl Overlay for $ty {
                fn overlay(&mut self, over: &Self) {
                    self.clone_from(over);
                }
            }
        )+
    };
}

replaced_whole!(String, u32, i64, f64, bool, Vec<String>);

macro_rules! section {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$fmeta:meta])* $field:ident : $ty:ty ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        pub struct $name {
            $(
                $(#[$fmeta])*
                #[serde(skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )+
        }

        impl Overlay for $name {
            fn overlay(&mut self, over: &Self) {
                $( self.$field.overlay(&over.$field); )+
            }
        }
    };
}

section! {
    /// `[meta]`
    MetaDoc {
        name: String,
        schema: u32,
        author: String,
        version: String,
        scheme: String,
        extends: String,
    }
}

section! {
    /// `[palette]`
    PaletteDoc {
        bg: String,
        surface: String,
        #[serde(rename = "surface-raised")]
        surface_raised: String,
        overlay: String,
        border: String,
        fg: String,
        #[serde(rename = "fg-muted")]
        fg_muted: String,
        accent: String,
        #[serde(rename = "accent-fg")]
        accent_fg: String,
        focus: String,
        success: String,
        warning: String,
        danger: String,
        progress: String,
        badge: String,
        kind: KindDoc,
    }
}

section! {
    /// `[palette.kind]`
    KindDoc {
        movie: String,
        show: String,
        music: String,
        book: String,
        live: String,
    }
}

section! {
    /// `[type]`
    TypeDoc {
        ui: String,
        display: String,
        scale: f64,
        base_px: i64,
        mono_ui: bool,
    }
}

section! {
    /// `[shape]`
    ShapeDoc {
        radius_px: i64,
        border_px: i64,
        density: String,
    }
}

section! {
    /// `[motion]`
    MotionDoc {
        duration_ms: i64,
        easing: String,
        focus_scale: f64,
    }
}

section! {
    /// `[effects]`
    EffectsDoc {
        background: String,
        shader: String,
        blur_px: i64,
        vignette: f64,
        grain: f64,
        glow: f64,
    }
}

section! {
    /// `[layout]`
    LayoutDoc {
        hero: bool,
        rail_poster: String,
        rail_collapsed: bool,
    }
}

section! {
    /// `[contrast]`
    ContrastDoc {
        level: String,
        decorative: Vec<String>,
    }
}

/// A theme file, schema 1.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThemeDoc {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<MetaDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<PaletteDoc>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<TypeDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub motion: Option<MotionDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<EffectsDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<ContrastDoc>,
}

impl ThemeDoc {
    /// `meta.name`, trimmed.
    pub fn name(&self) -> Option<&str> {
        self.meta.as_ref()?.name.as_deref().map(str::trim)
    }

    /// `meta.extends`, trimmed.
    pub fn extends(&self) -> Option<&str> {
        self.meta.as_ref()?.extends.as_deref().map(str::trim)
    }

    /// Replace every field of `self` that `over` sets, section by section and
    /// field by field. `contrast.decorative` is replaced whole.
    pub fn overlay(&mut self, over: &ThemeDoc) {
        self.meta.overlay(&over.meta);
        self.palette.overlay(&over.palette);
        self.r#type.overlay(&over.r#type);
        self.shape.overlay(&over.shape);
        self.motion.overlay(&over.motion);
        self.effects.overlay(&over.effects);
        self.layout.overlay(&over.layout);
        self.contrast.overlay(&over.contrast);
    }
}

impl PaletteDoc {
    /// The raw colour for `role`.
    pub fn get(&self, role: PaletteRole) -> Option<&String> {
        match role {
            PaletteRole::Bg => self.bg.as_ref(),
            PaletteRole::Surface => self.surface.as_ref(),
            PaletteRole::SurfaceRaised => self.surface_raised.as_ref(),
            PaletteRole::Overlay => self.overlay.as_ref(),
            PaletteRole::Border => self.border.as_ref(),
            PaletteRole::Fg => self.fg.as_ref(),
            PaletteRole::FgMuted => self.fg_muted.as_ref(),
            PaletteRole::Accent => self.accent.as_ref(),
            PaletteRole::AccentFg => self.accent_fg.as_ref(),
            PaletteRole::Focus => self.focus.as_ref(),
            PaletteRole::Success => self.success.as_ref(),
            PaletteRole::Warning => self.warning.as_ref(),
            PaletteRole::Danger => self.danger.as_ref(),
            PaletteRole::Progress => self.progress.as_ref(),
            PaletteRole::Badge => self.badge.as_ref(),
        }
    }
}

impl KindDoc {
    /// The raw colour for `kind`.
    pub fn get(&self, kind: Kind) -> Option<&String> {
        match kind {
            Kind::Movie => self.movie.as_ref(),
            Kind::Show => self.show.as_ref(),
            Kind::Music => self.music.as_ref(),
            Kind::Book => self.book.as_ref(),
            Kind::Live => self.live.as_ref(),
        }
    }
}

pub(super) fn clip(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(CLIP_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::theme::{format, SHIPPED};

    fn night() -> ThemeDoc {
        format::read(SHIPPED[0]).unwrap().0
    }

    fn named(name: &str) -> ThemeDoc {
        ThemeDoc {
            meta: Some(MetaDoc {
                name: Some(name.to_owned()),
                ..MetaDoc::default()
            }),
            ..ThemeDoc::default()
        }
    }

    #[test]
    fn overlay_replaces_only_the_fields_that_are_set() {
        let mut doc = night();
        let before = doc.clone();
        let mut over = named("Mine");
        over.palette = Some(PaletteDoc {
            accent: Some("#ff00ff".to_owned()),
            ..PaletteDoc::default()
        });
        doc.overlay(&over);

        let palette = doc.palette.as_ref().unwrap();
        let was = before.palette.as_ref().unwrap();
        assert_eq!(palette.accent.as_deref(), Some("#ff00ff"));
        assert_eq!(palette.bg, was.bg);
        assert_eq!(palette.kind, was.kind);
        assert_eq!(doc.name(), Some("Mine"));
        assert_eq!(
            doc.meta.as_ref().unwrap().schema,
            before.meta.unwrap().schema
        );
        assert_eq!(doc.shape, before.shape);
        assert_eq!(doc.contrast, before.contrast);
    }

    #[test]
    fn overlay_onto_an_empty_document_copies_the_section() {
        let mut doc = ThemeDoc::default();
        doc.overlay(&night());
        assert_eq!(doc, night());
    }

    #[test]
    fn kind_accents_overlay_field_by_field() {
        let mut doc = night();
        let was = doc.palette.clone().unwrap().kind.unwrap();
        let over = ThemeDoc {
            palette: Some(PaletteDoc {
                kind: Some(KindDoc {
                    music: Some("#00ff00".to_owned()),
                    ..KindDoc::default()
                }),
                ..PaletteDoc::default()
            }),
            ..ThemeDoc::default()
        };
        doc.overlay(&over);

        let kind = doc.palette.unwrap().kind.unwrap();
        assert_eq!(kind.music.as_deref(), Some("#00ff00"));
        assert_eq!(kind.movie, was.movie);
        assert_eq!(kind.show, was.show);
        assert_eq!(kind.book, was.book);
        assert_eq!(kind.live, was.live);
    }

    #[test]
    fn decorative_is_replaced_whole_not_appended() {
        let mut doc = night();
        doc.contrast = Some(ContrastDoc {
            level: Some("aa".to_owned()),
            decorative: Some(vec!["badge".to_owned(), "border".to_owned()]),
        });
        let over = ThemeDoc {
            contrast: Some(ContrastDoc {
                level: None,
                decorative: Some(vec!["progress".to_owned()]),
            }),
            ..ThemeDoc::default()
        };
        doc.overlay(&over);

        let contrast = doc.contrast.unwrap();
        assert_eq!(contrast.decorative, Some(vec!["progress".to_owned()]));
        assert_eq!(contrast.level.as_deref(), Some("aa"));
    }
}
