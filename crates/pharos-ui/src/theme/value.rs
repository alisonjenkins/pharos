//! The typed values a theme document is made of: colours, font stacks, the
//! closed enums, and the numeric bounds a document is checked against.

/// A closed set of spellings a document may use for a value.
pub trait Choice: Sized {
    fn parse(s: &str) -> Option<Self>;
    /// The accepted spellings, for an error message.
    fn choices() -> String;
}

/// A colour as 8-bit sRGB channels plus alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// Parse `#rgb`, `#rrggbb` or `#rrggbbaa`, in either case.
    pub fn parse(s: &str) -> Option<Self> {
        let hex = s.strip_prefix('#')?;
        let digits = hex
            .chars()
            .map(|c| c.to_digit(16).and_then(|d| u8::try_from(d).ok()))
            .collect::<Option<Vec<u8>>>()?;
        let pair = |hi: u8, lo: u8| (hi << 4) | lo;
        match digits.as_slice() {
            [r, g, b] => Some(Self {
                r: pair(*r, *r),
                g: pair(*g, *g),
                b: pair(*b, *b),
                a: 255,
            }),
            [r1, r2, g1, g2, b1, b2] => Some(Self {
                r: pair(*r1, *r2),
                g: pair(*g1, *g2),
                b: pair(*b1, *b2),
                a: 255,
            }),
            [r1, r2, g1, g2, b1, b2, a1, a2] => Some(Self {
                r: pair(*r1, *r2),
                g: pair(*g1, *g2),
                b: pair(*b1, *b2),
                a: pair(*a1, *a2),
            }),
            _ => None,
        }
    }

    /// Lowercase `#rrggbb`, or `#rrggbbaa` when not fully opaque.
    pub fn to_css(&self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

label_enum! {
    /// A named system font stack.
    FontStack {
        SystemUi => "system-ui",
        Sans => "sans",
        Serif => "serif",
        Mono => "mono",
    }
}

impl FontStack {
    /// The CSS `font-family` list for this stack.
    pub fn css(&self) -> &'static str {
        match self {
            Self::SystemUi => "system-ui, -apple-system, \"Segoe UI\", sans-serif",
            Self::Sans => "\"Inter\", \"Helvetica Neue\", Arial, sans-serif",
            Self::Serif => "Georgia, \"Times New Roman\", serif",
            Self::Mono => "ui-monospace, \"JetBrains Mono\", Menlo, Consolas, monospace",
        }
    }
}

label_enum! {
    /// Whether the theme is a dark or a light one (`color-scheme`).
    Scheme {
        Dark => "dark",
        Light => "light",
    }
}

label_enum! {
    /// How tightly the layout is spaced.
    Density {
        Comfortable => "comfortable",
        Compact => "compact",
    }
}

label_enum! {
    /// The motion easing curve.
    Easing {
        Standard => "standard",
        Emphasised => "emphasised",
        Linear => "linear",
    }
}

impl Easing {
    /// The CSS `<easing-function>`.
    pub fn css(&self) -> &'static str {
        match self {
            Self::Standard => "cubic-bezier(0.2, 0, 0, 1)",
            Self::Emphasised => "cubic-bezier(0.3, 0, 0, 1.3)",
            Self::Linear => "linear",
        }
    }
}

label_enum! {
    /// What fills the page behind the content. `image` waits for uploads.
    Background {
        Solid => "solid",
        Gradient => "gradient",
        Shader => "shader",
    }
}

label_enum! {
    /// An allow-listed background shader.
    ShaderName {
        Scanlines => "scanlines",
        MatrixRain => "matrix-rain",
        Aurora => "aurora",
        Grain => "grain",
    }
}

label_enum! {
    /// The shape of a poster on a rail.
    RailPoster {
        Portrait => "portrait",
        Landscape => "landscape",
        Numbered => "numbered",
    }
}

label_enum! {
    /// The WCAG conformance level a theme is held to.
    ContrastLevel {
        Aa => "aa",
        Aaa => "aaa",
    }
}

/// An inclusive numeric range a document value must fall in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub path: &'static str,
    pub min: f64,
    pub max: f64,
}

impl Bounds {
    pub fn contains(&self, value: f64) -> bool {
        (self.min..=self.max).contains(&value)
    }
}

pub const TYPE_SCALE: Bounds = Bounds {
    path: "type.scale",
    min: 1.0,
    max: 1.6,
};
pub const TYPE_BASE_PX: Bounds = Bounds {
    path: "type.base_px",
    min: 12.0,
    max: 24.0,
};
pub const SHAPE_RADIUS_PX: Bounds = Bounds {
    path: "shape.radius_px",
    min: 0.0,
    max: 32.0,
};
pub const SHAPE_BORDER_PX: Bounds = Bounds {
    path: "shape.border_px",
    min: 0.0,
    max: 4.0,
};
pub const MOTION_DURATION_MS: Bounds = Bounds {
    path: "motion.duration_ms",
    min: 0.0,
    max: 1000.0,
};
pub const MOTION_FOCUS_SCALE: Bounds = Bounds {
    path: "motion.focus_scale",
    min: 1.0,
    max: 1.2,
};
pub const EFFECTS_BLUR_PX: Bounds = Bounds {
    path: "effects.blur_px",
    min: 0.0,
    max: 64.0,
};
pub const EFFECTS_VIGNETTE: Bounds = Bounds {
    path: "effects.vignette",
    min: 0.0,
    max: 1.0,
};
pub const EFFECTS_GRAIN: Bounds = Bounds {
    path: "effects.grain",
    min: 0.0,
    max: 1.0,
};
pub const EFFECTS_GLOW: Bounds = Bounds {
    path: "effects.glow",
    min: 0.0,
    max: 1.0,
};

/// Every bounded number in a document.
pub const BOUNDS: [Bounds; 10] = [
    TYPE_SCALE,
    TYPE_BASE_PX,
    SHAPE_RADIUS_PX,
    SHAPE_BORDER_PX,
    MOTION_DURATION_MS,
    MOTION_FOCUS_SCALE,
    EFFECTS_BLUR_PX,
    EFFECTS_VIGNETTE,
    EFFECTS_GRAIN,
    EFFECTS_GLOW,
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn short_long_and_alpha_hex_colours_parse() {
        let cases = [
            (
                "#fff",
                Rgba {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
            ),
            (
                "#1a2",
                Rgba {
                    r: 0x11,
                    g: 0xaa,
                    b: 0x22,
                    a: 255,
                },
            ),
            (
                "#0e1116",
                Rgba {
                    r: 0x0e,
                    g: 0x11,
                    b: 0x16,
                    a: 255,
                },
            ),
            (
                "#0E1116",
                Rgba {
                    r: 0x0e,
                    g: 0x11,
                    b: 0x16,
                    a: 255,
                },
            ),
            (
                "#11151cee",
                Rgba {
                    r: 0x11,
                    g: 0x15,
                    b: 0x1c,
                    a: 0xee,
                },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(Rgba::parse(input), Some(expected), "{input}");
        }
    }

    #[test]
    fn colours_round_trip_to_lowercase_css() {
        let cases = [
            ("#fff", "#ffffff"),
            ("#0E1116", "#0e1116"),
            ("#11151cEE", "#11151cee"),
            ("#336699ff", "#336699"),
        ];
        for (input, css) in cases {
            let parsed = Rgba::parse(input).unwrap();
            assert_eq!(parsed.to_css(), css, "{input}");
            assert_eq!(Rgba::parse(&parsed.to_css()), Some(parsed), "{input}");
        }
    }

    #[test]
    fn malformed_colours_are_rejected() {
        for input in [
            "red", "#12", "#gggggg", "", "#", "ffffff", "#12345", "#1234", " #fff", "#ééé",
        ] {
            assert_eq!(Rgba::parse(input), None, "{input:?}");
        }
    }

    #[test]
    fn every_enum_label_parses_back_to_its_variant() {
        for stack in FontStack::ALL {
            assert_eq!(FontStack::parse(stack.label()), Some(*stack));
        }
        for shader in ShaderName::ALL {
            assert_eq!(ShaderName::parse(shader.label()), Some(*shader));
        }
        assert_eq!(Scheme::parse("dark"), Some(Scheme::Dark));
        assert_eq!(Density::parse("compact"), Some(Density::Compact));
        assert_eq!(Easing::parse("emphasised"), Some(Easing::Emphasised));
        assert_eq!(RailPoster::parse("numbered"), Some(RailPoster::Numbered));
        assert_eq!(ContrastLevel::parse("aaa"), Some(ContrastLevel::Aaa));
    }

    #[test]
    fn image_is_not_a_background_in_schema_one() {
        assert_eq!(Background::parse("image"), None);
        assert_eq!(Background::parse("shader"), Some(Background::Shader));
    }

    #[test]
    fn labels_are_case_sensitive() {
        assert_eq!(Scheme::parse("Dark"), None);
        assert_eq!(FontStack::parse("Mono"), None);
    }

    #[test]
    fn font_and_easing_css_match_the_contract() {
        assert_eq!(
            FontStack::SystemUi.css(),
            "system-ui, -apple-system, \"Segoe UI\", sans-serif"
        );
        assert_eq!(Easing::Standard.css(), "cubic-bezier(0.2, 0, 0, 1)");
        assert_eq!(Easing::Linear.css(), "linear");
    }

    #[test]
    fn bounds_are_inclusive_and_reject_nan() {
        assert!(TYPE_SCALE.contains(1.0));
        assert!(TYPE_SCALE.contains(1.6));
        assert!(!TYPE_SCALE.contains(1.61));
        assert!(!TYPE_SCALE.contains(f64::NAN));
    }
}
