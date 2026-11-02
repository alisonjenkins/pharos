//! WCAG 2.2 contrast between a theme's colour roles.

use std::fmt;

use super::resolve::{PaletteRole, Theme};
use super::value::{ContrastLevel, Rgba};

/// A foreground/background role pair and the ratio each level demands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastPair {
    pub fg: PaletteRole,
    pub bg: PaletteRole,
    pub min_aa: f64,
    pub min_aaa: f64,
    pub text: bool,
}

impl ContrastPair {
    /// The ratio this pair must reach at `level`.
    pub fn min(&self, level: ContrastLevel) -> f64 {
        match level {
            ContrastLevel::Aa => self.min_aa,
            ContrastLevel::Aaa => self.min_aaa,
        }
    }
}

impl fmt::Display for ContrastPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.fg.label(), self.bg.label())
    }
}

const fn text(fg: PaletteRole, bg: PaletteRole) -> ContrastPair {
    ContrastPair {
        fg,
        bg,
        min_aa: 4.5,
        min_aaa: 7.0,
        text: true,
    }
}

const fn non_text(fg: PaletteRole, bg: PaletteRole) -> ContrastPair {
    ContrastPair {
        fg,
        bg,
        min_aa: 3.0,
        min_aaa: 3.0,
        text: false,
    }
}

/// Every pair a theme is checked on. `border` is left out: it is decorative
/// by nature.
pub const PAIRS: [ContrastPair; 12] = [
    text(PaletteRole::Fg, PaletteRole::Bg),
    text(PaletteRole::Fg, PaletteRole::Surface),
    text(PaletteRole::Fg, PaletteRole::SurfaceRaised),
    text(PaletteRole::FgMuted, PaletteRole::Bg),
    text(PaletteRole::FgMuted, PaletteRole::Surface),
    text(PaletteRole::AccentFg, PaletteRole::Accent),
    text(PaletteRole::Fg, PaletteRole::Overlay),
    non_text(PaletteRole::Focus, PaletteRole::Bg),
    non_text(PaletteRole::Focus, PaletteRole::Surface),
    non_text(PaletteRole::Accent, PaletteRole::Bg),
    non_text(PaletteRole::Progress, PaletteRole::Surface),
    non_text(PaletteRole::Badge, PaletteRole::Surface),
];

/// One pair measured against its minimum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastResult {
    pub pair: ContrastPair,
    pub ratio: f64,
    pub min: f64,
    pub pass: bool,
    pub decorative: bool,
}

fn channel(c: f64) -> f64 {
    let c = c / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// The contrast ratio of `fg` on `bg`, from 1 to 21. A translucent `fg` is
/// composited over `bg`, which is treated as opaque.
pub fn ratio(fg: Rgba, bg: Rgba) -> f64 {
    let alpha = f64::from(fg.a) / 255.0;
    let mix = |f: u8, b: u8| f64::from(f) * alpha + f64::from(b) * (1.0 - alpha);
    let fg_lum = luminance(mix(fg.r, bg.r), mix(fg.g, bg.g), mix(fg.b, bg.b));
    let bg_lum = luminance(f64::from(bg.r), f64::from(bg.g), f64::from(bg.b));
    let (lighter, darker) = if fg_lum >= bg_lum {
        (fg_lum, bg_lum)
    } else {
        (bg_lum, fg_lum)
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// Every pair in `PAIRS` measured at the theme's contrast level.
pub fn check(theme: &Theme) -> Vec<ContrastResult> {
    PAIRS
        .iter()
        .map(|pair| {
            let measured = ratio(theme.palette.get(pair.fg), theme.palette.get(pair.bg));
            let min = pair.min(theme.contrast);
            ContrastResult {
                pair: *pair,
                ratio: measured,
                min,
                pass: measured >= min,
                decorative: !pair.text && theme.decorative.contains(&pair.fg),
            }
        })
        .collect()
}

/// The pairs below their minimum that the theme has not waived.
pub fn failures(theme: &Theme) -> Vec<ContrastResult> {
    check(theme)
        .into_iter()
        .filter(|r| !r.pass && !r.decorative)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::theme::{shipped, DEFAULT};

    const BLACK: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    const WHITE: Rgba = Rgba {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    fn night() -> Theme {
        shipped().resolve(DEFAULT).unwrap().theme
    }

    #[test]
    fn black_on_white_is_twenty_one_to_one_either_way_round() {
        assert!((ratio(BLACK, WHITE) - 21.0).abs() < 0.01);
        assert!((ratio(WHITE, BLACK) - 21.0).abs() < 0.01);
    }

    #[test]
    fn mid_grey_777777_on_white_is_about_4_48() {
        let grey = Rgba::parse("#777777").unwrap();
        assert!((ratio(grey, WHITE) - 4.48).abs() < 0.01);
    }

    #[test]
    fn identical_colours_have_a_ratio_of_one() {
        let c = Rgba::parse("#4cb4ff").unwrap();
        assert!((ratio(c, c) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_translucent_foreground_is_composited_over_the_background() {
        let transparent_white = Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 0,
        };
        assert!((ratio(transparent_white, BLACK) - 1.0).abs() < 1e-9);
        let faint = Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 128,
        };
        let solid_grey = Rgba {
            r: 128,
            g: 128,
            b: 128,
            a: 255,
        };
        assert!((ratio(faint, BLACK) - ratio(solid_grey, BLACK)).abs() < 0.01);
    }

    #[test]
    fn a_pair_is_text_exactly_when_its_foreground_role_is() {
        for pair in PAIRS {
            assert_eq!(pair.text, pair.fg.is_text(), "{pair}");
        }
    }

    #[test]
    fn aaa_never_asks_for_less_than_aa() {
        for pair in PAIRS {
            assert!(pair.min_aaa >= pair.min_aa, "{pair}");
        }
    }

    #[test]
    fn check_reports_every_pair_at_the_themes_level() {
        let mut theme = night();
        assert_eq!(check(&theme).len(), PAIRS.len());
        assert!(check(&theme).iter().all(|r| r.min <= 4.5));
        theme.contrast = ContrastLevel::Aaa;
        let fg_bg = check(&theme)
            .into_iter()
            .find(|r| r.pair.fg == PaletteRole::Fg && r.pair.bg == PaletteRole::Bg)
            .unwrap();
        assert_eq!(fg_bg.min, 7.0);
    }

    #[test]
    fn a_failing_text_pair_is_reported_and_cannot_be_waived() {
        let mut theme = night();
        theme.palette.fg = theme.palette.bg;
        let failed: Vec<_> = failures(&theme)
            .iter()
            .map(|r| r.pair.to_string())
            .collect();
        assert!(failed.contains(&"fg/bg".to_owned()), "{failed:?}");
    }

    #[test]
    fn a_waived_non_text_pair_is_listed_but_not_a_failure() {
        let mut theme = night();
        theme.palette.badge = theme.palette.surface;
        let failing_badge = |t: &Theme| {
            check(t)
                .into_iter()
                .find(|r| r.pair.fg == PaletteRole::Badge)
                .unwrap()
        };
        assert!(!failing_badge(&theme).pass);
        assert!(failures(&theme)
            .iter()
            .any(|r| r.pair.fg == PaletteRole::Badge));

        theme.decorative = vec![PaletteRole::Badge];
        let waived = failing_badge(&theme);
        assert!(!waived.pass && waived.decorative);
        assert!(failures(&theme).is_empty());
    }
}
