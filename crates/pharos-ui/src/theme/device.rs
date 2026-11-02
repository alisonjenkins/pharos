//! The theme a device shows at start-up, from what its storage held, and
//! the console lines that say which one and why.

use super::resolve::{Catalogue, Theme, ThemeError};
use super::signal::{self, Source};
use super::{doc, DEFAULT};

/// The `localStorage` key holding the device's chosen theme name.
pub const ACTIVE_KEY: &str = "pharos.theme.active";

/// A console line and the level it is logged at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Info(String),
    Warn(String),
}

/// The theme to apply and what to log about it.
#[derive(Debug, Clone)]
pub struct Startup {
    pub theme: Theme,
    pub lines: Vec<Line>,
}

/// The device's theme: the stored name when it resolves, else the default
/// with a `fallback` line naming why. `Err` only when the default itself
/// does not resolve, which the shipped-theme tests rule out.
pub fn startup(
    catalogue: &Catalogue,
    stored: Result<Option<String>, String>,
) -> Result<Startup, ThemeError> {
    let mut lines = Vec::new();
    let failed = match stored {
        Ok(None) => None,
        Ok(Some(name)) if name == DEFAULT => None,
        Ok(Some(name)) => match apply(catalogue, &name, &mut lines) {
            Ok(theme) => return Ok(Startup { theme, lines }),
            Err(e) => Some((name, e)),
        },
        Err(message) => Some(("-".to_owned(), ThemeError::Storage { message })),
    };
    if let Some((name, error)) = failed {
        lines.push(Line::Warn(signal::fallback(
            &name,
            Source::Shipped,
            &error,
            DEFAULT,
        )));
    }
    let theme = apply(catalogue, DEFAULT, &mut lines)?;
    Ok(Startup { theme, lines })
}

fn apply(catalogue: &Catalogue, name: &str, lines: &mut Vec<Line>) -> Result<Theme, ThemeError> {
    if catalogue.get_source(name).is_none() {
        return Err(ThemeError::UnknownTheme {
            name: name.to_owned(),
        });
    }
    let resolved = catalogue.resolve(name)?;
    for warning in &resolved.warnings {
        lines.push(Line::Warn(signal::warning(name, Source::Shipped, warning)));
    }
    lines.push(Line::Info(signal::applied(
        name,
        Source::Shipped,
        doc::SCHEMA,
        resolved.warnings.len(),
    )));
    Ok(resolved.theme)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::theme::shipped;

    fn run(stored: Result<Option<String>, String>) -> Startup {
        startup(&shipped(), stored).unwrap()
    }

    #[test]
    fn nothing_stored_applies_the_default_and_says_so() {
        let s = run(Ok(None));
        assert_eq!(s.theme.name, DEFAULT);
        assert_eq!(
            s.lines,
            [Line::Info(
                "pharos: theme applied name=\"Pharos Night\" source=shipped schema=1 warnings=0"
                    .into()
            )]
        );
    }

    #[test]
    fn a_stored_shipped_name_is_applied() {
        let s = run(Ok(Some("Neon Grid".into())));
        assert_eq!(s.theme.name, "Neon Grid");
        assert_eq!(
            s.lines,
            [Line::Info(
                "pharos: theme applied name=\"Neon Grid\" source=shipped schema=1 warnings=0"
                    .into()
            )]
        );
    }

    #[test]
    fn an_unknown_stored_name_falls_back_naming_it() {
        let s = run(Ok(Some("Glitter".into())));
        assert_eq!(s.theme.name, DEFAULT);
        assert_eq!(
            s.lines.first(),
            Some(&Line::Warn(
                "pharos: theme fallback name=Glitter source=shipped reason=unknown_theme path=- value=Glitter to=\"Pharos Night\""
                    .into()
            ))
        );
        assert!(
            matches!(s.lines.get(1), Some(Line::Info(l)) if l.contains("applied name=\"Pharos Night\""))
        );
    }

    #[test]
    fn unreadable_storage_falls_back_with_the_browser_message() {
        let s = run(Err("SecurityError: access denied".into()));
        assert_eq!(s.theme.name, DEFAULT);
        assert_eq!(
            s.lines.first(),
            Some(&Line::Warn(
                "pharos: theme fallback name=- source=shipped reason=storage path=- value=\"SecurityError: access denied\" to=\"Pharos Night\""
                    .into()
            ))
        );
    }

    #[test]
    fn a_default_that_does_not_resolve_is_an_error_not_a_panic() {
        assert!(matches!(
            startup(&Catalogue::default(), Ok(None)),
            Err(ThemeError::UnknownTheme { .. })
        ));
    }
}
