//! The `pharos` theme: faces from the V508 font pack, colours and outlines
//! (spec 028 FR-020). Colours are ASS `&HAABBGGRR` words; alpha 00 is opaque.

pub mod names {
    pub const DEFAULT: &str = "Default";
    pub const SHOUT: &str = "Shout";
    pub const SCREAM: &str = "Scream";
    pub const WHISPER: &str = "Whisper";
    pub const THOUGHT: &str = "Thought";
    pub const ECHO: &str = "Echo";
    pub const WIRE: &str = "Wire";
    pub const MEMORY: &str = "Memory";
    pub const CAPTION: &str = "Caption";
    pub const ASIDE: &str = "Aside";
    pub const BURST: &str = "Burst";
    pub const GIGGLE: &str = "Giggle";
    pub const KARAOKE: &str = "Karaoke";
}

/// The faces in the font pack (V508) and the dialogue face.
pub mod faces {
    pub const DIALOGUE: &str = "Liberation Sans";
    pub const DISPLAY: &str = "Bangers";
    pub const CONDENSED: &str = "Anton";
    pub const HAND: &str = "Patrick Hand";
}

/// Wire's text colour, a cool silver-blue.
pub const WIRE_PRIMARY: u32 = rgb(200, 225, 255);

/// An opaque `&H00BBGGRR` colour from RGB.
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((b as u32) << 16) | ((g as u32) << 8) | r as u32
}

/// `colour` with ASS alpha `a` (0 opaque, 255 clear).
pub const fn with_alpha(colour: u32, a: u8) -> u32 {
    (colour & 0x00FF_FFFF) | ((a as u32) << 24)
}

/// The colour as an override-tag argument, `&HBBGGRR&`.
pub fn tag_colour(colour: u32) -> String {
    format!("&H{:06X}&", colour & 0x00FF_FFFF)
}

#[derive(Clone, Debug, PartialEq)]
pub struct StyleDef {
    pub name: String,
    pub font: &'static str,
    pub size: u32,
    pub primary: u32,
    pub secondary: u32,
    pub outline_colour: u32,
    pub back: u32,
    pub bold: bool,
    pub italic: bool,
    pub spacing: f32,
    /// 1 outline and shadow, 3 opaque box (drawn in `outline_colour`).
    pub border_style: u8,
    pub outline: f32,
    pub shadow: f32,
    /// Numpad alignment.
    pub alignment: u8,
    pub margin_v: i32,
}

impl StyleDef {
    fn base(name: &str) -> StyleDef {
        StyleDef {
            name: name.to_owned(),
            font: faces::DIALOGUE,
            size: 56,
            primary: rgb(255, 255, 255),
            secondary: rgb(255, 255, 255),
            outline_colour: rgb(0, 0, 0),
            back: with_alpha(rgb(0, 0, 0), 0x80),
            bold: false,
            italic: false,
            spacing: 0.0,
            border_style: 1,
            outline: 3.0,
            shadow: 1.0,
            alignment: 2,
            margin_v: 54,
        }
    }

    /// One `Style:` line in the order of [`STYLE_FORMAT`].
    pub(crate) fn render(&self) -> String {
        let flag = |b: bool| if b { -1 } else { 0 };
        format!(
            "Style: {},{},{},&H{:08X},&H{:08X},&H{:08X},&H{:08X},{},{},0,0,100,100,{},0,{},{},{},{},30,30,{},1",
            self.name,
            self.font,
            self.size,
            self.primary,
            self.secondary,
            self.outline_colour,
            self.back,
            flag(self.bold),
            flag(self.italic),
            self.spacing,
            self.border_style,
            self.outline,
            self.shadow,
            self.alignment,
            self.margin_v,
        )
    }
}

pub(crate) const STYLE_FORMAT: &str = "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding";

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub play_res_x: i32,
    pub play_res_y: i32,
    /// `Default` first, then one style per effect that needs its own face.
    pub styles: Vec<StyleDef>,
    /// What [`scaled`](Self::scaled) multiplied the styles by, so the
    /// compiler scales the size tags its effects write inline to match.
    pub tag_scale: f32,
}

impl Theme {
    pub fn pharos() -> Theme {
        use names::*;
        let dialogue = StyleDef::base(DEFAULT);
        let shout = StyleDef {
            size: 66,
            bold: true,
            primary: rgb(255, 242, 176),
            outline_colour: rgb(42, 10, 0),
            outline: 4.0,
            ..StyleDef::base(SHOUT)
        };
        let scream = StyleDef {
            font: faces::DISPLAY,
            size: 78,
            primary: rgb(255, 224, 216),
            outline_colour: rgb(92, 10, 10),
            outline: 5.0,
            shadow: 2.0,
            ..StyleDef::base(SCREAM)
        };
        let whisper = StyleDef {
            size: 40,
            primary: rgb(200, 212, 228),
            outline_colour: rgb(30, 36, 48),
            outline: 2.0,
            shadow: 0.0,
            ..StyleDef::base(WHISPER)
        };
        let thought = StyleDef {
            font: faces::HAND,
            size: 56,
            italic: true,
            primary: rgb(230, 220, 255),
            outline_colour: rgb(42, 34, 80),
            outline: 3.0,
            ..StyleDef::base(THOUGHT)
        };
        let echo = StyleDef {
            font: faces::DISPLAY,
            size: 72,
            primary: rgb(255, 255, 255),
            outline_colour: rgb(26, 58, 138),
            outline: 4.0,
            ..StyleDef::base(ECHO)
        };
        // The dialogue face and size in a cool silver-blue with a soft dark
        // outline: it reads as a voice from elsewhere without a box (B662).
        let wire = StyleDef {
            primary: WIRE_PRIMARY,
            outline_colour: with_alpha(rgb(10, 20, 38), 0x30),
            back: with_alpha(rgb(0, 6, 20), 0xA0),
            outline: 2.5,
            shadow: 1.5,
            ..StyleDef::base(WIRE)
        };
        let memory = StyleDef {
            size: 50,
            italic: true,
            primary: rgb(220, 200, 180),
            outline_colour: rgb(74, 58, 40),
            spacing: 1.0,
            ..StyleDef::base(MEMORY)
        };
        let caption = StyleDef {
            font: faces::CONDENSED,
            size: 50,
            outline_colour: with_alpha(rgb(16, 16, 16), 0x30),
            border_style: 3,
            outline: 10.0,
            shadow: 0.0,
            alignment: 8,
            margin_v: 40,
            ..StyleDef::base(CAPTION)
        };
        let aside = StyleDef {
            size: 46,
            italic: true,
            primary: rgb(190, 190, 190),
            outline_colour: rgb(32, 32, 32),
            ..StyleDef::base(ASIDE)
        };
        let burst = StyleDef {
            font: faces::DISPLAY,
            size: 64,
            primary: rgb(255, 194, 74),
            outline_colour: rgb(90, 40, 0),
            outline: 5.0,
            shadow: 2.0,
            alignment: 8,
            margin_v: 120,
            ..StyleDef::base(BURST)
        };
        // Laughter: the rounded hand face in a warm pink with a deep rose
        // outline, at the dialogue's spot (B1205).
        let giggle = StyleDef {
            font: faces::HAND,
            size: 76,
            primary: rgb(255, 204, 228),
            outline_colour: rgb(118, 22, 70),
            outline: 4.0,
            shadow: 1.5,
            ..StyleDef::base(GIGGLE)
        };
        // Sung text takes the primary colour, unsung the secondary.
        let karaoke = StyleDef {
            size: 58,
            bold: true,
            primary: rgb(255, 216, 74),
            secondary: rgb(208, 216, 232),
            outline_colour: rgb(32, 32, 48),
            ..StyleDef::base(KARAOKE)
        };
        Theme {
            play_res_x: 1920,
            play_res_y: 1080,
            styles: vec![
                dialogue, shout, scream, whisper, thought, echo, wire, memory, caption, aside,
                burst, giggle, karaoke,
            ],
            tag_scale: 1.0,
        }
    }

    pub fn style(&self, name: &str) -> Option<&StyleDef> {
        self.styles.iter().find(|s| s.name == name)
    }

    /// The theme with every style's text, outline and shadow at `percent` of
    /// its size: the viewer's subtitle size choice (spec 028 FR-022). Margins
    /// stay, so the text still sits where the plain track's does. The
    /// compiler scales the effects' inline size tags by the same factor.
    pub fn scaled(&self, percent: u16) -> Theme {
        let k = f32::from(percent) / 100.0;
        let mut out = self.clone();
        for s in &mut out.styles {
            s.size = (s.size as f32 * k).round() as u32;
            s.outline *= k;
            s.shadow *= k;
        }
        out.tag_scale *= k;
        out
    }

    /// Where `\an2` text sits by default: bottom centre.
    pub fn bottom_anchor(&self) -> (i32, i32) {
        let mv = self.style(names::DEFAULT).map_or(54, |s| s.margin_v);
        (self.play_res_x / 2, self.play_res_y - mv)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(rgb(1, 2, 3), 0x0003_0201);
        assert_eq!(with_alpha(rgb(1, 2, 3), 0x80), 0x8003_0201);
        assert_eq!(tag_colour(with_alpha(rgb(255, 0, 16), 0x40)), "&H1000FF&");
    }

    #[test]
    fn faces_are_the_font_pack() {
        let t = Theme::pharos();
        let used: std::collections::BTreeSet<_> = t.styles.iter().map(|s| s.font).collect();
        assert_eq!(
            used.into_iter().collect::<Vec<_>>(),
            ["Anton", "Bangers", "Liberation Sans", "Patrick Hand"]
        );
        assert_eq!(t.styles[0].name, "Default");
        assert_eq!((t.play_res_x, t.play_res_y), (1920, 1080));
    }

    /// B662: Wire is the dialogue face and size with an outline and shadow,
    /// not a monospaced box.
    #[test]
    fn wire_is_dialogue_shaped_in_silver_blue() {
        let t = Theme::pharos();
        let (wire, dialogue) = (t.style(names::WIRE).unwrap(), &t.styles[0]);
        assert_eq!((wire.font, wire.size), (dialogue.font, dialogue.size));
        assert_eq!(wire.primary, rgb(200, 225, 255));
        assert_eq!(wire.border_style, 1);
        assert!(wire.outline > 0.0 && wire.outline <= dialogue.outline);
        assert!(wire.shadow > 0.0 && wire.shadow <= 2.0);
        assert!(wire
            .render()
            .starts_with("Style: Wire,Liberation Sans,56,&H00FFE1C8,"));
    }

    #[test]
    fn scaled_grows_text_outline_and_shadow_but_not_margins() {
        let t = Theme::pharos();
        assert_eq!(t.scaled(100), t);
        let big = t.scaled(150);
        for (a, b) in t.styles.iter().zip(&big.styles) {
            assert_eq!(b.size, (a.size as f32 * 1.5).round() as u32, "{}", a.name);
            assert!((b.outline - a.outline * 1.5).abs() < 1e-4, "{}", a.name);
            assert!((b.shadow - a.shadow * 1.5).abs() < 1e-4, "{}", a.name);
            assert_eq!(b.margin_v, a.margin_v, "{}", a.name);
        }
        assert_eq!(t.scaled(75).styles[0].size, 42);
    }

    #[test]
    fn style_names_are_unique_and_lines_have_23_fields() {
        let t = Theme::pharos();
        let names: std::collections::BTreeSet<_> =
            t.styles.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), t.styles.len());
        let fmt_fields = STYLE_FORMAT
            .trim_start_matches("Format: ")
            .split(',')
            .count();
        for s in &t.styles {
            let line = s.render();
            assert_eq!(
                line.trim_start_matches("Style: ").split(',').count(),
                fmt_fields
            );
        }
    }

    #[test]
    fn bottom_anchor_follows_the_default_margin() {
        assert_eq!(Theme::pharos().bottom_anchor(), (960, 1026));
    }
}
