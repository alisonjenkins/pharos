//! The `pharos: theme …` console lines, in the `key=value` shape the
//! playback logs use.

use super::contrast::ContrastResult;
use super::doc;
use super::resolve::{ThemeError, ThemeWarning};

/// Where a theme came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Shipped,
    Local,
    Import,
    Editor,
    Auto,
}

impl Source {
    /// The spelling used in log lines.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Shipped => "shipped",
            Self::Local => "local",
            Self::Import => "import",
            Self::Editor => "editor",
            Self::Auto => "auto",
        }
    }
}

fn field(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '=');
    if needs_quotes {
        format!("{value:?}")
    } else {
        value.to_owned()
    }
}

/// A theme was applied.
pub fn applied(name: &str, source: Source, schema: u32, warnings: usize) -> String {
    format!(
        "pharos: theme applied name={} source={} schema={schema} warnings={warnings}",
        field(name),
        source.label()
    )
}

/// A theme loaded with a warning.
pub fn warning(name: &str, source: Source, warning: &ThemeWarning) -> String {
    let (path, value) = warning.path_value();
    format!(
        "pharos: theme warning name={} source={} reason={} path={} value={}",
        field(name),
        source.label(),
        warning.label(),
        field(&path),
        field(&value)
    )
}

/// A theme was rejected and `to` applied instead.
pub fn fallback(name: &str, source: Source, error: &ThemeError, to: &str) -> String {
    let (path, value) = error.path_value();
    format!(
        "pharos: theme fallback name={} source={} reason={} path={} value={} to={}",
        field(name),
        source.label(),
        error.label(),
        field(&path),
        field(&value),
        field(to)
    )
}

/// A custom theme was saved, with `warnings` warnings.
pub fn saved(name: &str, source: Source, warnings: usize) -> String {
    format!(
        "pharos: theme saved name={} source={} schema={} warnings={warnings}",
        field(name),
        source.label(),
        doc::SCHEMA
    )
}

/// A theme was refused on save or import for failing a contrast pair.
pub fn rejected(name: &str, source: Source, result: &ContrastResult) -> String {
    format!(
        "pharos: theme rejected name={} source={} reason=contrast pair={} ratio={:.2} min={}",
        field(name),
        source.label(),
        result.pair,
        result.ratio,
        result.min
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use super::*;
    use crate::theme::contrast::PAIRS;

    #[test]
    fn applied_line_has_the_documented_shape() {
        assert_eq!(
            applied("Pharos Night", Source::Shipped, 1, 0),
            "pharos: theme applied name=\"Pharos Night\" source=shipped schema=1 warnings=0"
        );
    }

    #[test]
    fn warning_line_names_reason_path_and_value() {
        let w = ThemeWarning::UnknownKey {
            path: "palette.glitter".into(),
        };
        assert_eq!(
            warning("Mine", Source::Import, &w),
            "pharos: theme warning name=Mine source=import reason=unknown_key path=palette.glitter value=-"
        );
        let w = ThemeWarning::FontMissing {
            path: "type.ui".into(),
            value: "Comic Sans".into(),
        };
        assert_eq!(
            warning("Mine", Source::Local, &w),
            "pharos: theme warning name=Mine source=local reason=font_missing path=type.ui value=\"Comic Sans\""
        );
    }

    #[test]
    fn fallback_line_names_the_error_and_the_default() {
        let e = ThemeError::OutOfRange {
            path: "shape.radius_px".into(),
            value: 99.0,
            min: 0.0,
            max: 32.0,
        };
        assert_eq!(
            fallback("Mine", Source::Local, &e, "Pharos Night"),
            "pharos: theme fallback name=Mine source=local reason=out_of_range path=shape.radius_px value=99 to=\"Pharos Night\""
        );
    }

    #[test]
    fn an_error_without_a_path_uses_a_dash_and_the_chain() {
        let e = ThemeError::ExtendsCycle {
            chain: vec!["A".into(), "B".into(), "A".into()],
        };
        assert_eq!(
            fallback("A", Source::Import, &e, "Pharos Night"),
            "pharos: theme fallback name=A source=import reason=extends_cycle path=- value=A>B>A to=\"Pharos Night\""
        );
    }

    #[test]
    fn saved_line_has_the_documented_shape() {
        assert_eq!(
            saved("Mine", Source::Editor, 2),
            "pharos: theme saved name=Mine source=editor schema=1 warnings=2"
        );
    }

    #[test]
    fn rejected_line_gives_the_pair_ratio_to_two_places_and_minimum() {
        let result = ContrastResult {
            pair: PAIRS[0],
            ratio: 3.456,
            min: 4.5,
            pass: false,
            decorative: false,
        };
        assert_eq!(
            rejected("Mine", Source::Editor, &result),
            "pharos: theme rejected name=Mine source=editor reason=contrast pair=fg/bg ratio=3.46 min=4.5"
        );
    }

    #[test]
    fn source_labels_are_distinct() {
        let labels: HashSet<_> = [
            Source::Shipped,
            Source::Local,
            Source::Import,
            Source::Editor,
            Source::Auto,
        ]
        .iter()
        .map(Source::label)
        .collect();
        assert_eq!(labels.len(), 5);
    }

    #[test]
    fn every_error_and_warning_label_is_distinct() {
        let pair = PAIRS[0];
        let errors = [
            ThemeError::Syntax {
                message: String::new(),
            },
            ThemeError::MissingKey {
                path: String::new(),
            },
            ThemeError::BadValue {
                path: String::new(),
                value: String::new(),
                expected: String::new(),
            },
            ThemeError::OutOfRange {
                path: String::new(),
                value: 0.0,
                min: 0.0,
                max: 0.0,
            },
            ThemeError::UnknownBase {
                name: String::new(),
            },
            ThemeError::ExtendsCycle { chain: vec![] },
            ThemeError::ExtendsTooDeep { chain: vec![] },
            ThemeError::TooLarge { bytes: 0 },
            ThemeError::Contrast {
                pair,
                ratio: 0.0,
                min: 0.0,
            },
            ThemeError::UnknownTheme {
                name: String::new(),
            },
            ThemeError::Storage {
                message: String::new(),
            },
        ];
        let warnings = [
            ThemeWarning::UnknownKey {
                path: String::new(),
            },
            ThemeWarning::NewerSchema {
                found: 0,
                supported: 0,
            },
            ThemeWarning::FontMissing {
                path: String::new(),
                value: String::new(),
            },
            ThemeWarning::EffectsReduced {
                reason: String::new(),
            },
        ];
        let labels: HashSet<&str> = errors
            .iter()
            .map(ThemeError::label)
            .chain(warnings.iter().map(ThemeWarning::label))
            .collect();
        assert_eq!(labels.len(), errors.len() + warnings.len());
        for stable in [
            "syntax",
            "bad_value",
            "out_of_range",
            "missing_key",
            "unknown_base",
            "extends_cycle",
            "extends_too_deep",
            "too_large",
            "contrast",
            "unknown_theme",
            "storage",
            "unknown_key",
            "newer_schema",
            "font_missing",
            "effects_reduced",
        ] {
            assert!(labels.contains(stable), "{stable}");
        }
    }
}
