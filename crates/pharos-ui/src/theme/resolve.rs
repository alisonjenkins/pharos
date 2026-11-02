//! Turning a theme file into a `Theme` with nothing optional: follow
//! `extends`, merge field by field, then check every value and name the path
//! of the first one that is wrong.

use thiserror::Error;

use super::contrast::ContrastPair;
use super::doc::{self, clip, ThemeDoc};
use super::format;
use super::value::{
    Background, Bounds, Choice, ContrastLevel, Density, Easing, FontStack, RailPoster, Rgba,
    Scheme, ShaderName, EFFECTS_BLUR_PX, EFFECTS_GLOW, EFFECTS_GRAIN, EFFECTS_VIGNETTE,
    MOTION_DURATION_MS, MOTION_FOCUS_SCALE, SHAPE_BORDER_PX, SHAPE_RADIUS_PX, TYPE_BASE_PX,
    TYPE_SCALE,
};
use super::DEFAULT;

/// Why a document is rejected.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ThemeError {
    #[error("syntax error: {message}")]
    Syntax { message: String },
    #[error("{path}: missing required key")]
    MissingKey { path: String },
    #[error("{path}: bad value {value:?}, expected {expected}")]
    BadValue {
        path: String,
        value: String,
        expected: String,
    },
    #[error("{path}: value {value} is outside {min} to {max}")]
    OutOfRange {
        path: String,
        value: f64,
        min: f64,
        max: f64,
    },
    #[error("meta.extends: unknown base theme {name:?}")]
    UnknownBase { name: String },
    #[error("meta.extends: cycle {}", .chain.join(" > "))]
    ExtendsCycle { chain: Vec<String> },
    #[error("document: {bytes} bytes is over the {} byte limit", doc::MAX_BYTES)]
    TooLarge { bytes: usize },
    #[error("contrast {pair}: ratio {ratio:.2} is below the minimum {min}")]
    Contrast {
        pair: ContrastPair,
        ratio: f64,
        min: f64,
    },
    #[error("no theme is named {name:?}")]
    UnknownTheme { name: String },
    #[error("theme storage unreadable: {message}")]
    Storage { message: String },
}

impl ThemeError {
    /// A stable reason string for logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Syntax { .. } => "syntax",
            Self::MissingKey { .. } => "missing_key",
            Self::BadValue { .. } => "bad_value",
            Self::OutOfRange { .. } => "out_of_range",
            Self::UnknownBase { .. } => "unknown_base",
            Self::ExtendsCycle { .. } => "extends_cycle",
            Self::TooLarge { .. } => "too_large",
            Self::Contrast { .. } => "contrast",
            Self::UnknownTheme { .. } => "unknown_theme",
            Self::Storage { .. } => "storage",
        }
    }

    /// The TOML path and offending value, `-` where the error has none.
    pub fn path_value(&self) -> (String, String) {
        let none = || "-".to_owned();
        match self {
            Self::Syntax { message } => (none(), message.clone()),
            Self::MissingKey { path } => (path.clone(), none()),
            Self::BadValue { path, value, .. } => (path.clone(), value.clone()),
            Self::OutOfRange { path, value, .. } => (path.clone(), value.to_string()),
            Self::UnknownBase { name } => ("meta.extends".to_owned(), name.clone()),
            Self::ExtendsCycle { chain } => (none(), chain.join(">")),
            Self::TooLarge { bytes } => (none(), bytes.to_string()),
            Self::Contrast { pair, ratio, .. } => (none(), format!("{pair}={ratio:.2}")),
            Self::UnknownTheme { name } => (none(), name.clone()),
            Self::Storage { message } => (none(), message.clone()),
        }
    }
}

/// Something wrong that does not stop the document loading.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ThemeWarning {
    #[error("{path}: unknown key ignored")]
    UnknownKey { path: String },
    #[error("meta.schema: {found} is newer than the supported {supported}")]
    NewerSchema { found: u32, supported: u32 },
    #[error("{path}: font {value:?} is not available, using system-ui")]
    FontMissing { path: String, value: String },
    #[error("effects: reduced ({reason})")]
    EffectsReduced { reason: String },
}

impl ThemeWarning {
    /// A stable reason string for logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::UnknownKey { .. } => "unknown_key",
            Self::NewerSchema { .. } => "newer_schema",
            Self::FontMissing { .. } => "font_missing",
            Self::EffectsReduced { .. } => "effects_reduced",
        }
    }

    /// The TOML path and offending value, `-` where the warning has none.
    pub fn path_value(&self) -> (String, String) {
        match self {
            Self::UnknownKey { path } => (path.clone(), "-".to_owned()),
            Self::NewerSchema { found, .. } => ("meta.schema".to_owned(), found.to_string()),
            Self::FontMissing { path, value } => (path.clone(), value.clone()),
            Self::EffectsReduced { reason } => ("effects".to_owned(), reason.clone()),
        }
    }
}

label_enum! {
    /// A colour role every theme defines.
    PaletteRole {
        Bg => "bg",
        Surface => "surface",
        SurfaceRaised => "surface-raised",
        Overlay => "overlay",
        Border => "border",
        Fg => "fg",
        FgMuted => "fg-muted",
        Accent => "accent",
        AccentFg => "accent-fg",
        Focus => "focus",
        Success => "success",
        Warning => "warning",
        Danger => "danger",
        Progress => "progress",
        Badge => "badge",
    }
}

impl PaletteRole {
    /// Whether the role colours text, and so cannot be waived as decorative.
    pub fn is_text(&self) -> bool {
        matches!(self, Self::Fg | Self::FgMuted | Self::AccentFg)
    }
}

label_enum! {
    /// A kind of library item with its own accent.
    Kind {
        Movie => "movie",
        Show => "show",
        Music => "music",
        Book => "book",
        Live => "live",
    }
}

/// One colour per palette role.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub bg: Rgba,
    pub surface: Rgba,
    pub surface_raised: Rgba,
    pub overlay: Rgba,
    pub border: Rgba,
    pub fg: Rgba,
    pub fg_muted: Rgba,
    pub accent: Rgba,
    pub accent_fg: Rgba,
    pub focus: Rgba,
    pub success: Rgba,
    pub warning: Rgba,
    pub danger: Rgba,
    pub progress: Rgba,
    pub badge: Rgba,
}

impl Palette {
    /// The colour for `role`.
    pub fn get(&self, role: PaletteRole) -> Rgba {
        match role {
            PaletteRole::Bg => self.bg,
            PaletteRole::Surface => self.surface,
            PaletteRole::SurfaceRaised => self.surface_raised,
            PaletteRole::Overlay => self.overlay,
            PaletteRole::Border => self.border,
            PaletteRole::Fg => self.fg,
            PaletteRole::FgMuted => self.fg_muted,
            PaletteRole::Accent => self.accent,
            PaletteRole::AccentFg => self.accent_fg,
            PaletteRole::Focus => self.focus,
            PaletteRole::Success => self.success,
            PaletteRole::Warning => self.warning,
            PaletteRole::Danger => self.danger,
            PaletteRole::Progress => self.progress,
            PaletteRole::Badge => self.badge,
        }
    }
}

/// One accent per item kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KindAccents {
    pub movie: Rgba,
    pub show: Rgba,
    pub music: Rgba,
    pub book: Rgba,
    pub live: Rgba,
}

impl KindAccents {
    /// The accent for `kind`.
    pub fn get(&self, kind: Kind) -> Rgba {
        match kind {
            Kind::Movie => self.movie,
            Kind::Show => self.show,
            Kind::Music => self.music,
            Kind::Book => self.book,
            Kind::Live => self.live,
        }
    }
}

/// Background and effect settings; amounts are within their bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effects {
    pub background: Background,
    pub shader: Option<ShaderName>,
    pub blur_px: u32,
    pub vignette: f64,
    pub grain: f64,
    pub glow: f64,
}

/// Page layout choices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub hero: bool,
    pub rail_poster: RailPoster,
    pub rail_collapsed: bool,
}

/// A fully resolved theme: every value present and within its bounds.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub scheme: Scheme,
    pub palette: Palette,
    pub kind: KindAccents,
    pub ui: FontStack,
    pub display: FontStack,
    pub scale: f64,
    pub base_px: u32,
    pub mono_ui: bool,
    pub radius_px: u32,
    pub border_px: u32,
    pub density: Density,
    pub duration_ms: u32,
    pub easing: Easing,
    pub focus_scale: f64,
    pub effects: Effects,
    pub layout: Layout,
    pub contrast: ContrastLevel,
    pub decorative: Vec<PaletteRole>,
}

/// A resolved theme and the warnings met on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub theme: Theme,
    pub warnings: Vec<ThemeWarning>,
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    doc: ThemeDoc,
    source: String,
    warnings: Vec<ThemeWarning>,
}

/// Named theme documents with their source text.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    entries: Vec<Entry>,
}

impl Catalogue {
    /// Parse `source` and store it under its `meta.name`, replacing a theme
    /// of the same name. Returns the warnings the document loaded with.
    pub fn insert(&mut self, source: String) -> Result<Vec<ThemeWarning>, ThemeError> {
        let (doc, warnings) = format::read(&source)?;
        let name = doc
            .name()
            .ok_or_else(|| ThemeError::MissingKey {
                path: "meta.name".to_owned(),
            })?
            .to_owned();
        let entry = Entry {
            name,
            doc,
            source,
            warnings: warnings.clone(),
        };
        match self.entries.iter_mut().find(|e| e.name == entry.name) {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
        Ok(warnings)
    }

    /// The text `name` was inserted as, byte for byte.
    pub fn get_source(&self, name: &str) -> Option<&str> {
        self.find(name).map(|e| e.source.as_str())
    }

    /// Drop `name`, returning whether it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        self.entries.len() != before
    }

    /// Theme names in insertion order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Resolve `name` through its `extends` chain.
    pub fn resolve(&self, name: &str) -> Result<Resolved, ThemeError> {
        let chain = self.chain(name)?;
        let mut doc = ThemeDoc::default();
        for entry in chain.iter().rev() {
            doc.overlay(&entry.doc);
        }

        let mut warnings = chain
            .first()
            .map(|leaf| leaf.warnings.clone())
            .unwrap_or_default();
        let theme = build(name, &doc, &mut warnings)?;
        Ok(Resolved { theme, warnings })
    }

    fn find(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    fn chain(&self, name: &str) -> Result<Vec<&Entry>, ThemeError> {
        let mut names: Vec<String> = Vec::new();
        let mut entries = Vec::new();
        let mut current = name.to_owned();
        loop {
            if names.contains(&current) {
                names.push(current);
                return Err(ThemeError::ExtendsCycle { chain: names });
            }
            let Some(entry) = self.find(&current) else {
                return Err(ThemeError::UnknownBase { name: current });
            };
            names.push(current);
            entries.push(entry);
            let base = if entry.name == DEFAULT {
                None
            } else {
                Some(entry.doc.extends().unwrap_or(DEFAULT))
            };
            match base {
                Some(base) => current = base.to_owned(),
                None => return Ok(entries),
            }
        }
    }
}

fn missing<T>(value: Option<T>, path: &str) -> Result<T, ThemeError> {
    value.ok_or_else(|| ThemeError::MissingKey {
        path: path.to_owned(),
    })
}

fn bad(path: &str, value: &str, expected: impl Into<String>) -> ThemeError {
    ThemeError::BadValue {
        path: path.to_owned(),
        value: clip(value),
        expected: expected.into(),
    }
}

fn colour(value: Option<&String>, path: &str) -> Result<Rgba, ThemeError> {
    let raw = missing(value, path)?;
    Rgba::parse(raw).ok_or_else(|| bad(path, raw, "a colour: #rgb, #rrggbb or #rrggbbaa"))
}

fn pick<T: Choice>(value: Option<&String>, path: &str) -> Result<T, ThemeError> {
    let raw = missing(value, path)?;
    T::parse(raw).ok_or_else(|| bad(path, raw, format!("one of {}", T::choices())))
}

fn float(value: Option<f64>, bounds: &Bounds) -> Result<f64, ThemeError> {
    let value = missing(value, bounds.path)?;
    if bounds.contains(value) {
        Ok(value)
    } else {
        Err(out_of_range(bounds, value))
    }
}

fn whole(value: Option<i64>, bounds: &Bounds) -> Result<u32, ThemeError> {
    let raw = missing(value, bounds.path)?;
    let as_float = raw as f64;
    if !bounds.contains(as_float) {
        return Err(out_of_range(bounds, as_float));
    }
    u32::try_from(raw).map_err(|_| out_of_range(bounds, as_float))
}

fn out_of_range(bounds: &Bounds, value: f64) -> ThemeError {
    ThemeError::OutOfRange {
        path: bounds.path.to_owned(),
        value,
        min: bounds.min,
        max: bounds.max,
    }
}

fn font(
    value: Option<&String>,
    path: &str,
    warnings: &mut Vec<ThemeWarning>,
) -> Result<FontStack, ThemeError> {
    let raw = missing(value, path)?;
    Ok(FontStack::parse(raw).unwrap_or_else(|| {
        warnings.push(ThemeWarning::FontMissing {
            path: path.to_owned(),
            value: clip(raw),
        });
        FontStack::SystemUi
    }))
}

fn build_palette(doc: &super::doc::PaletteDoc) -> Result<Palette, ThemeError> {
    let c = |role: PaletteRole| colour(doc.get(role), &format!("palette.{}", role.label()));
    Ok(Palette {
        bg: c(PaletteRole::Bg)?,
        surface: c(PaletteRole::Surface)?,
        surface_raised: c(PaletteRole::SurfaceRaised)?,
        overlay: c(PaletteRole::Overlay)?,
        border: c(PaletteRole::Border)?,
        fg: c(PaletteRole::Fg)?,
        fg_muted: c(PaletteRole::FgMuted)?,
        accent: c(PaletteRole::Accent)?,
        accent_fg: c(PaletteRole::AccentFg)?,
        focus: c(PaletteRole::Focus)?,
        success: c(PaletteRole::Success)?,
        warning: c(PaletteRole::Warning)?,
        danger: c(PaletteRole::Danger)?,
        progress: c(PaletteRole::Progress)?,
        badge: c(PaletteRole::Badge)?,
    })
}

fn build_kinds(doc: &super::doc::KindDoc) -> Result<KindAccents, ThemeError> {
    let c = |kind: Kind| colour(doc.get(kind), &format!("palette.kind.{}", kind.label()));
    Ok(KindAccents {
        movie: c(Kind::Movie)?,
        show: c(Kind::Show)?,
        music: c(Kind::Music)?,
        book: c(Kind::Book)?,
        live: c(Kind::Live)?,
    })
}

fn build_decorative(raw: Option<&Vec<String>>) -> Result<Vec<PaletteRole>, ThemeError> {
    let mut roles = Vec::new();
    for (i, item) in missing(raw, "contrast.decorative")?.iter().enumerate() {
        let path = format!("contrast.decorative[{i}]");
        let role = PaletteRole::parse(item)
            .ok_or_else(|| bad(&path, item, format!("one of {}", PaletteRole::choices())))?;
        if role.is_text() {
            return Err(bad(&path, item, "a non-text role"));
        }
        if !roles.contains(&role) {
            roles.push(role);
        }
    }
    Ok(roles)
}

fn build(
    name: &str,
    doc: &ThemeDoc,
    warnings: &mut Vec<ThemeWarning>,
) -> Result<Theme, ThemeError> {
    let meta = doc.meta.clone().unwrap_or_default();
    let palette = doc.palette.clone().unwrap_or_default();
    let typography = doc.r#type.clone().unwrap_or_default();
    let shape = doc.shape.clone().unwrap_or_default();
    let motion = doc.motion.clone().unwrap_or_default();
    let effects = doc.effects.clone().unwrap_or_default();
    let layout = doc.layout.clone().unwrap_or_default();
    let contrast = doc.contrast.clone().unwrap_or_default();

    let scheme = pick(meta.scheme.as_ref(), "meta.scheme")?;
    let colours = build_palette(&palette)?;
    let kind = build_kinds(&palette.kind.clone().unwrap_or_default())?;

    let ui = font(typography.ui.as_ref(), "type.ui", warnings)?;
    let display = font(typography.display.as_ref(), "type.display", warnings)?;
    let scale = float(typography.scale, &TYPE_SCALE)?;
    let base_px = whole(typography.base_px, &TYPE_BASE_PX)?;
    let mono_ui = missing(typography.mono_ui, "type.mono_ui")?;

    let radius_px = whole(shape.radius_px, &SHAPE_RADIUS_PX)?;
    let border_px = whole(shape.border_px, &SHAPE_BORDER_PX)?;
    let density = pick(shape.density.as_ref(), "shape.density")?;

    let duration_ms = whole(motion.duration_ms, &MOTION_DURATION_MS)?;
    let easing = pick(motion.easing.as_ref(), "motion.easing")?;
    let focus_scale = float(motion.focus_scale, &MOTION_FOCUS_SCALE)?;

    let background: Background = pick(effects.background.as_ref(), "effects.background")?;
    let named_shader = effects
        .shader
        .as_ref()
        .map(|s| pick::<ShaderName>(Some(s), "effects.shader"))
        .transpose()?;
    let shader = if background == Background::Shader {
        Some(missing(named_shader, "effects.shader")?)
    } else {
        None
    };
    let effects = Effects {
        background,
        shader,
        blur_px: whole(effects.blur_px, &EFFECTS_BLUR_PX)?,
        vignette: float(effects.vignette, &EFFECTS_VIGNETTE)?,
        grain: float(effects.grain, &EFFECTS_GRAIN)?,
        glow: float(effects.glow, &EFFECTS_GLOW)?,
    };

    let layout = Layout {
        hero: missing(layout.hero, "layout.hero")?,
        rail_poster: pick(layout.rail_poster.as_ref(), "layout.rail_poster")?,
        rail_collapsed: missing(layout.rail_collapsed, "layout.rail_collapsed")?,
    };

    let level = pick(contrast.level.as_ref(), "contrast.level")?;
    let decorative = build_decorative(contrast.decorative.as_ref())?;

    Ok(Theme {
        name: name.to_owned(),
        scheme,
        palette: colours,
        kind,
        ui,
        display,
        scale,
        base_px,
        mono_ui,
        radius_px,
        border_px,
        density,
        duration_ms,
        easing,
        focus_scale,
        effects,
        layout,
        contrast: level,
        decorative,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::theme::value::{
        BOUNDS, EFFECTS_BLUR_PX, EFFECTS_GLOW, EFFECTS_GRAIN, EFFECTS_VIGNETTE, MOTION_DURATION_MS,
        MOTION_FOCUS_SCALE, SHAPE_BORDER_PX, SHAPE_RADIUS_PX, TYPE_BASE_PX, TYPE_SCALE,
    };
    use crate::theme::{contrast, shipped, SHIPPED};

    fn document(name: &str, extends: Option<&str>, rest: &str) -> String {
        let extends = extends.map_or(String::new(), |e| format!("extends = \"{e}\"\n"));
        format!("[meta]\nname = \"{name}\"\nschema = 1\n{extends}{rest}")
    }

    fn resolve_child(rest: &str) -> Result<Resolved, ThemeError> {
        let mut catalogue = shipped();
        catalogue.insert(document("Child", None, rest))?;
        catalogue.resolve("Child")
    }

    fn night() -> Theme {
        shipped().resolve(DEFAULT).unwrap().theme
    }

    #[test]
    fn a_theme_with_only_a_name_is_pharos_night_under_another_name() {
        let child = resolve_child("").unwrap();
        assert_eq!(child.warnings, vec![]);
        assert_eq!(
            child.theme,
            Theme {
                name: "Child".into(),
                ..night()
            }
        );
    }

    #[test]
    fn the_child_wins_field_by_field_and_the_rest_is_inherited() {
        let child =
            resolve_child("[palette]\naccent = \"#ff00ff\"\n[shape]\nradius_px = 3\n").unwrap();
        let base = night();
        assert_eq!(child.theme.palette.accent, Rgba::parse("#ff00ff").unwrap());
        assert_eq!(child.theme.radius_px, 3);
        assert_eq!(child.theme.palette.bg, base.palette.bg);
        assert_eq!(child.theme.border_px, base.border_px);
        assert_eq!(child.theme.kind, base.kind);
    }

    #[test]
    fn a_chain_of_three_merges_root_first() {
        let mut catalogue = shipped();
        catalogue
            .insert(document(
                "Mid",
                Some("Marquee"),
                "[shape]\nradius_px = 20\n",
            ))
            .unwrap();
        catalogue
            .insert(document("Leaf", Some("Mid"), "[shape]\nborder_px = 2\n"))
            .unwrap();
        let leaf = catalogue.resolve("Leaf").unwrap().theme;
        let marquee = catalogue.resolve("Marquee").unwrap().theme;
        assert_eq!(leaf.radius_px, 20);
        assert_eq!(leaf.border_px, 2);
        assert_eq!(leaf.palette.accent, marquee.palette.accent);
        assert_eq!(leaf.layout.rail_poster, RailPoster::Numbered);
    }

    #[test]
    fn pharos_night_needs_every_key_from_its_own_file() {
        let root = SHIPPED[0];
        let mut section = String::new();
        let mut checked = 0;
        for (i, line) in root.lines().enumerate() {
            let trimmed = line.trim();
            if let Some(name) = trimmed.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.to_owned();
                continue;
            }
            let Some((key, _)) = trimmed.split_once('=') else {
                continue;
            };
            let path = format!("{section}.{}", key.trim());
            if matches!(
                path.as_str(),
                "meta.author" | "meta.version" | "meta.extends"
            ) {
                continue;
            }
            let without: String = root
                .lines()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, l)| format!("{l}\n"))
                .collect();
            let mut catalogue = Catalogue::default();
            let err = match catalogue.insert(without) {
                Err(e) => e,
                Ok(_) => catalogue.resolve(DEFAULT).unwrap_err(),
            };
            assert_eq!(err, ThemeError::MissingKey { path: path.clone() }, "{path}");
            checked += 1;
        }
        assert!(checked >= 44, "only {checked} keys checked");
    }

    #[test]
    fn the_first_missing_key_in_file_order_is_the_one_named() {
        let src = SHIPPED[0]
            .lines()
            .filter(|l| !l.starts_with("surface =") && !l.starts_with("radius_px"))
            .map(|l| format!("{l}\n"))
            .collect::<String>();
        let mut catalogue = Catalogue::default();
        catalogue.insert(src).unwrap();
        assert_eq!(
            catalogue.resolve(DEFAULT).unwrap_err(),
            ThemeError::MissingKey {
                path: "palette.surface".into()
            }
        );
    }

    #[test]
    fn unknown_keys_load_with_a_warning_on_the_resolved_theme() {
        let resolved = resolve_child("[palette]\nglitter = \"#fff\"\n[nope]\nx = 1\n").unwrap();
        assert_eq!(
            resolved.warnings,
            vec![
                ThemeWarning::UnknownKey {
                    path: "nope".into()
                },
                ThemeWarning::UnknownKey {
                    path: "palette.glitter".into()
                },
            ]
        );
    }

    #[test]
    fn a_newer_schema_loads_and_a_zero_schema_does_not() {
        let mut catalogue = shipped();
        let newer = catalogue
            .insert("[meta]\nname = \"N\"\nschema = 2\n".into())
            .unwrap();
        assert_eq!(
            newer,
            vec![ThemeWarning::NewerSchema {
                found: 2,
                supported: 1
            }]
        );
        assert_eq!(
            catalogue.resolve("N").unwrap().warnings,
            vec![ThemeWarning::NewerSchema {
                found: 2,
                supported: 1
            }]
        );
        let zero = catalogue.insert("[meta]\nname = \"Z\"\nschema = 0\n".into());
        assert!(matches!(zero, Err(ThemeError::BadValue { .. })));
    }

    #[test]
    fn a_radius_of_99_is_out_of_range_and_names_its_path() {
        let err = resolve_child("[shape]\nradius_px = 99\n").unwrap_err();
        assert_eq!(
            err,
            ThemeError::OutOfRange {
                path: "shape.radius_px".into(),
                value: 99.0,
                min: 0.0,
                max: 32.0
            }
        );
        let shown = err.to_string();
        assert!(
            shown.contains("shape.radius_px") && shown.contains("99"),
            "{shown}"
        );
    }

    #[test]
    fn every_bound_accepts_its_ends_and_rejects_just_outside() {
        for bounds in BOUNDS {
            let (section, key) = bounds.path.split_once('.').unwrap();
            for ok in [bounds.min, bounds.max] {
                let rest = format!("[{section}]\n{key} = {ok}\n");
                assert!(resolve_child(&rest).is_ok(), "{} = {ok}", bounds.path);
            }
            for outside in [bounds.min - 1.0, bounds.max + 1.0] {
                let rest = format!("[{section}]\n{key} = {outside}\n");
                assert_eq!(
                    resolve_child(&rest).unwrap_err(),
                    ThemeError::OutOfRange {
                        path: bounds.path.into(),
                        value: outside,
                        min: bounds.min,
                        max: bounds.max
                    },
                    "{} = {outside}",
                    bounds.path
                );
            }
        }
    }

    #[test]
    fn the_bounds_table_is_the_one_set_of_documented_ranges() {
        let table: Vec<_> = [
            (TYPE_SCALE, 1.0, 1.6),
            (TYPE_BASE_PX, 12.0, 24.0),
            (SHAPE_RADIUS_PX, 0.0, 32.0),
            (SHAPE_BORDER_PX, 0.0, 4.0),
            (MOTION_DURATION_MS, 0.0, 1000.0),
            (MOTION_FOCUS_SCALE, 1.0, 1.2),
            (EFFECTS_BLUR_PX, 0.0, 64.0),
            (EFFECTS_VIGNETTE, 0.0, 1.0),
            (EFFECTS_GRAIN, 0.0, 1.0),
            (EFFECTS_GLOW, 0.0, 1.0),
        ]
        .into_iter()
        .map(|(b, min, max)| (b.path, b.min, b.max, min, max))
        .collect();
        for (path, bmin, bmax, min, max) in table {
            assert_eq!((bmin, bmax), (min, max), "{path}");
        }
        assert_eq!(BOUNDS.len(), 10);
    }

    #[test]
    fn a_bad_colour_names_its_path_and_the_value() {
        let err = resolve_child("[palette]\naccent = \"blue\"\n").unwrap_err();
        let ThemeError::BadValue { path, value, .. } = &err else {
            panic!("expected BadValue, got {err:?}");
        };
        assert_eq!((path.as_str(), value.as_str()), ("palette.accent", "blue"));
        let shown = err.to_string();
        assert!(
            shown.contains("palette.accent") && shown.contains("blue"),
            "{shown}"
        );
    }

    #[test]
    fn an_unknown_enum_choice_is_a_bad_value_listing_the_choices() {
        let err = resolve_child("[shape]\ndensity = \"roomy\"\n").unwrap_err();
        assert!(
            matches!(&err, ThemeError::BadValue { path, value, expected }
                if path == "shape.density" && value == "roomy" && expected.contains("comfortable")),
            "{err:?}"
        );
    }

    #[test]
    fn background_image_is_a_bad_value_in_schema_one() {
        let err = resolve_child("[effects]\nbackground = \"image\"\n").unwrap_err();
        assert!(
            matches!(&err, ThemeError::BadValue { path, value, .. }
                if path == "effects.background" && value == "image"),
            "{err:?}"
        );
    }

    #[test]
    fn a_shader_is_resolved_only_for_a_shader_background() {
        let neon = shipped().resolve("Neon Grid").unwrap().theme;
        assert_eq!(neon.effects.background, Background::Shader);
        assert_eq!(neon.effects.shader, Some(ShaderName::Scanlines));
        assert_eq!(night().effects.shader, None);

        let solid =
            resolve_child("[effects]\nbackground = \"solid\"\nshader = \"aurora\"\n").unwrap();
        assert_eq!(solid.theme.effects.shader, None);

        let missing_name = resolve_child("[effects]\nbackground = \"shader\"\n").unwrap_err();
        assert_eq!(
            missing_name,
            ThemeError::MissingKey {
                path: "effects.shader".into()
            }
        );

        let bad_name = resolve_child("[effects]\nshader = \"plasma\"\n").unwrap_err();
        assert!(
            matches!(bad_name, ThemeError::BadValue { .. }),
            "{bad_name:?}"
        );
    }

    #[test]
    fn extends_cycles_are_reported_with_the_chain() {
        let mut catalogue = shipped();
        catalogue.insert(document("A", Some("B"), "")).unwrap();
        catalogue.insert(document("B", Some("A"), "")).unwrap();
        assert_eq!(
            catalogue.resolve("A").unwrap_err(),
            ThemeError::ExtendsCycle {
                chain: vec!["A".into(), "B".into(), "A".into()]
            }
        );
        catalogue.insert(document("Me", Some("Me"), "")).unwrap();
        assert_eq!(
            catalogue.resolve("Me").unwrap_err(),
            ThemeError::ExtendsCycle {
                chain: vec!["Me".into(), "Me".into()]
            }
        );
    }

    #[test]
    fn an_unknown_base_is_named() {
        let mut catalogue = shipped();
        catalogue
            .insert(document("Orphan", Some("Nope"), ""))
            .unwrap();
        assert_eq!(
            catalogue.resolve("Orphan").unwrap_err(),
            ThemeError::UnknownBase {
                name: "Nope".into()
            }
        );
        assert_eq!(
            shipped().resolve("Nowhere").unwrap_err(),
            ThemeError::UnknownBase {
                name: "Nowhere".into()
            }
        );
    }

    #[test]
    fn without_pharos_night_nothing_resolves() {
        let mut catalogue = Catalogue::default();
        catalogue.insert(document("Lonely", None, "")).unwrap();
        assert_eq!(
            catalogue.resolve("Lonely").unwrap_err(),
            ThemeError::UnknownBase {
                name: DEFAULT.into()
            }
        );
    }

    #[test]
    fn an_unknown_font_warns_and_falls_back_to_system_ui() {
        let resolved = resolve_child("[type]\nui = \"Comic Sans\"\ndisplay = \"mono\"\n").unwrap();
        assert_eq!(resolved.theme.ui, FontStack::SystemUi);
        assert_eq!(resolved.theme.display, FontStack::Mono);
        assert_eq!(
            resolved.warnings,
            vec![ThemeWarning::FontMissing {
                path: "type.ui".into(),
                value: "Comic Sans".into()
            }]
        );
    }

    #[test]
    fn a_text_role_cannot_be_decorative() {
        for role in ["fg", "fg-muted", "accent-fg"] {
            let err =
                resolve_child(&format!("[contrast]\ndecorative = [\"{role}\"]\n")).unwrap_err();
            assert!(
                matches!(&err, ThemeError::BadValue { path, value, .. }
                    if path == "contrast.decorative[0]" && value == role),
                "{role}: {err:?}"
            );
        }
    }

    #[test]
    fn an_unknown_decorative_role_is_a_bad_value() {
        let err = resolve_child("[contrast]\ndecorative = [\"badge\", \"sparkle\"]\n").unwrap_err();
        assert!(
            matches!(&err, ThemeError::BadValue { path, value, .. }
                if path == "contrast.decorative[1]" && value == "sparkle"),
            "{err:?}"
        );
    }

    #[test]
    fn a_decorative_badge_is_excluded_from_contrast_failures() {
        let failing = "[palette]\nbadge = \"#1a2030\"\n";
        let strict = resolve_child(failing).unwrap().theme;
        assert!(contrast::failures(&strict)
            .iter()
            .any(|r| r.pair.fg == PaletteRole::Badge));

        let waived = resolve_child(&format!(
            "{failing}[contrast]\ndecorative = [\"badge\", \"badge\"]\n"
        ))
        .unwrap()
        .theme;
        assert_eq!(waived.decorative, vec![PaletteRole::Badge]);
        assert!(contrast::failures(&waived).is_empty());
    }

    #[test]
    fn a_catalogue_returns_the_inserted_source_byte_for_byte() {
        let source = document(
            "Odd",
            None,
            "# comment  \r\n[palette]\n  accent   =   \"#FFF\"   \n\n\n",
        );
        let mut catalogue = shipped();
        catalogue.insert(source.clone()).unwrap();
        assert_eq!(catalogue.get_source("Odd"), Some(source.as_str()));
        assert_eq!(catalogue.get_source("Missing"), None);
        assert_eq!(catalogue.get_source(DEFAULT), Some(SHIPPED[0]));
    }

    #[test]
    fn inserting_a_name_twice_replaces_it_in_place() {
        let mut catalogue = shipped();
        catalogue
            .insert(document("Marquee", None, "[shape]\nradius_px = 1\n"))
            .unwrap();
        assert_eq!(catalogue.names().filter(|n| *n == "Marquee").count(), 1);
        assert_eq!(catalogue.names().nth(1), Some("Marquee"));
        assert_eq!(catalogue.resolve("Marquee").unwrap().theme.radius_px, 1);
    }

    #[test]
    fn remove_drops_a_theme_and_says_whether_it_was_there() {
        let mut catalogue = shipped();
        assert!(catalogue.remove("Marquee"));
        assert!(!catalogue.remove("Marquee"));
        assert!(catalogue.get_source("Marquee").is_none());
        assert_eq!(catalogue.names().count(), 6);
    }

    #[test]
    fn a_rejected_insert_leaves_the_catalogue_unchanged() {
        let mut catalogue = shipped();
        assert!(catalogue.insert("[meta".into()).is_err());
        assert_eq!(catalogue.names().count(), 7);
    }

    #[test]
    fn error_messages_carry_their_path_and_value() {
        let cases: Vec<(ThemeError, Vec<&str>)> = vec![
            (
                ThemeError::MissingKey {
                    path: "palette.bg".into(),
                },
                vec!["palette.bg"],
            ),
            (
                ThemeError::BadValue {
                    path: "palette.fg".into(),
                    value: "zzz".into(),
                    expected: "a colour".into(),
                },
                vec!["palette.fg", "zzz"],
            ),
            (
                ThemeError::UnknownBase {
                    name: "Nope".into(),
                },
                vec!["meta.extends", "Nope"],
            ),
            (
                ThemeError::ExtendsCycle {
                    chain: vec!["A".into(), "B".into(), "A".into()],
                },
                vec!["A > B > A"],
            ),
            (
                ThemeError::TooLarge { bytes: 70000 },
                vec!["70000", "65536"],
            ),
        ];
        for (err, parts) in cases {
            let shown = err.to_string();
            for part in parts {
                assert!(shown.contains(part), "{shown} lacks {part}");
            }
        }
    }
}
