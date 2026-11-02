//! The theme engine (010): parse a theme document, resolve its `extends`
//! chain, check contrast and emit CSS custom properties. Nothing here touches
//! the browser, so all of it is tested on the host.

macro_rules! label_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $label:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The variant a document spells as `s`, if any.
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $( $label => Some(Self::$variant), )+
                    _ => None,
                }
            }

            /// The spelling used in documents and log lines.
            pub fn label(&self) -> &'static str {
                match self {
                    $( Self::$variant => $label, )+
                }
            }
        }

        impl $crate::theme::value::Choice for $name {
            fn parse(s: &str) -> Option<Self> {
                Self::parse(s)
            }

            fn choices() -> String {
                Self::ALL.iter().map(Self::label).collect::<Vec<_>>().join(" | ")
            }
        }
    };
}

pub mod contrast;
pub mod css;
pub mod device;
pub mod doc;
pub mod editor;
pub mod format;
pub mod resolve;
pub mod signal;
pub mod value;

pub use resolve::{Catalogue, Resolved, Theme, ThemeError, ThemeWarning};

/// The theme applied when nothing else is chosen, and the root every other
/// theme extends by default.
pub const DEFAULT: &str = "Pharos Night";

/// The seven themes built into the binary, in display order.
pub const SHIPPED: [&str; 7] = [
    include_str!("../../themes/pharos-night.pharos-theme.toml"),
    include_str!("../../themes/marquee.pharos-theme.toml"),
    include_str!("../../themes/kali-dark.pharos-theme.toml"),
    include_str!("../../themes/kali-purple.pharos-theme.toml"),
    include_str!("../../themes/neon-grid.pharos-theme.toml"),
    include_str!("../../themes/daylight.pharos-theme.toml"),
    include_str!("../../themes/high-contrast.pharos-theme.toml"),
];

const SHIPPED_NAMES: [&str; 7] = [
    "Pharos Night",
    "Marquee",
    "Kali Dark",
    "Kali Purple",
    "Neon Grid",
    "Daylight",
    "High Contrast",
];

/// Whether `name` is one of the seven built-in themes.
pub fn is_shipped(name: &str) -> bool {
    SHIPPED_NAMES.contains(&name)
}

/// A catalogue holding the shipped themes.
pub fn shipped() -> Catalogue {
    let mut catalogue = Catalogue::default();
    for source in SHIPPED {
        // A shipped theme that fails to parse is caught by this module's tests.
        let _ = catalogue.insert(source.to_owned());
    }
    catalogue
}

/// A lowercase ASCII identifier for a theme name, for element ids and file
/// names: every run of other characters becomes one `-`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "theme".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn slug_keeps_ascii_alphanumerics_and_collapses_the_rest() {
        assert_eq!(slug("../My \"Theme\" <x>"), "my-theme-x");
        assert_eq!(slug("Kali Dark"), "kali-dark");
        assert_eq!(slug("Neon  Grid 2"), "neon-grid-2");
        assert_eq!(slug("Café"), "caf");
        assert_eq!(slug("---"), "theme");
        assert_eq!(slug(""), "theme");
    }

    #[test]
    fn there_are_exactly_the_seven_shipped_themes() {
        let catalogue = shipped();
        assert_eq!(
            catalogue.names().collect::<Vec<_>>(),
            [
                "Pharos Night",
                "Marquee",
                "Kali Dark",
                "Kali Purple",
                "Neon Grid",
                "Daylight",
                "High Contrast"
            ]
        );
    }

    #[test]
    fn every_shipped_theme_resolves_with_no_warnings_and_passes_contrast() {
        let catalogue = shipped();
        for name in catalogue.names() {
            let resolved = catalogue.resolve(name).unwrap();
            assert_eq!(resolved.warnings, vec![], "{name}");
            assert_eq!(resolved.theme.name, name);
            assert!(contrast::failures(&resolved.theme).is_empty(), "{name}");
        }
    }

    #[test]
    fn the_shipped_name_list_matches_the_shipped_sources() {
        let names: Vec<String> = SHIPPED
            .iter()
            .map(|source| format::read(source).unwrap().0.name().unwrap().to_owned())
            .collect();
        assert_eq!(names, SHIPPED_NAMES);
        assert!(SHIPPED_NAMES.iter().all(|n| is_shipped(n)));
        assert!(!is_shipped("Mine"));
        assert!(!is_shipped("pharos night"));
    }

    #[test]
    fn the_default_theme_is_the_first_shipped_theme() {
        assert_eq!(shipped().names().next(), Some(DEFAULT));
    }
}
