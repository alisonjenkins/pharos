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
    pub const KARAOKE: &str = "Karaoke";
}

/// The faces in the font pack (V508) and the dialogue face.
pub mod faces {
    pub const DIALOGUE: &str = "Liberation Sans";
    pub const DISPLAY: &str = "Bangers";
    pub const CONDENSED: &str = "Anton";
    pub const HAND: &str = "Patrick Hand";
    pub const MONO: &str = "Courier Prime";
}

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
        let wire = StyleDef {
            font: faces::MONO,
            size: 44,
            primary: rgb(184, 255, 200),
            outline_colour: with_alpha(rgb(12, 20, 16), 0x40),
            border_style: 3,
            outline: 8.0,
            shadow: 0.0,
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
                burst, karaoke,
            ],
        }
    }

    pub fn style(&self, name: &str) -> Option<&StyleDef> {
        self.styles.iter().find(|s| s.name == name)
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
            [
                "Anton",
                "Bangers",
                "Courier Prime",
                "Liberation Sans",
                "Patrick Hand"
            ]
        );
        assert_eq!(t.styles[0].name, "Default");
        assert_eq!((t.play_res_x, t.play_res_y), (1920, 1080));
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
