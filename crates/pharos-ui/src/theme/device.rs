//! The theme a device shows at start-up, from what its storage held, the
//! custom themes it keeps, and the console lines that say which one and why.

use super::contrast;
use super::format;
use super::resolve::{Catalogue, Theme, ThemeError};
use super::signal::{self, Source};
use super::{doc, is_shipped, DEFAULT};

/// The `localStorage` key holding the device's chosen theme name.
pub const ACTIVE_KEY: &str = "pharos.theme.active";
/// The `localStorage` key holding a JSON array of custom theme files.
pub const CUSTOM_KEY: &str = "pharos.theme.custom";
/// The active-theme value that follows the system's colour and contrast preferences.
pub const AUTO: &str = "auto";
/// The most custom themes kept; a save past this is refused, a load keeps the first.
pub const MAX_CUSTOM: usize = 32;
/// The largest stored custom-themes JSON accepted, in bytes.
pub const MAX_CUSTOM_BYTES: usize = 512 * 1024;

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

/// A custom theme that was saved, and the lines that say so.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub name: String,
    pub lines: Vec<Line>,
}

/// A save that was refused: the lines that say why, and a sentence for the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub lines: Vec<Line>,
    pub reason: String,
}

/// The shipped theme `auto` resolves to for the system's preferences.
pub fn auto_name(prefers_dark: bool, prefers_more_contrast: bool) -> &'static str {
    if prefers_more_contrast {
        "High Contrast"
    } else if prefers_dark {
        "Pharos Night"
    } else {
        "Daylight"
    }
}

/// The device's theme: the stored name when it resolves, else the default
/// with a `fallback` line naming why. `Err` only when the default itself
/// does not resolve, which the shipped-theme tests rule out.
pub fn startup(
    catalogue: &Catalogue,
    stored: Result<Option<String>, String>,
    prefers_dark: bool,
    prefers_more_contrast: bool,
) -> Result<Startup, ThemeError> {
    let mut lines = Vec::new();
    let failed = match stored {
        Ok(None) => None,
        Ok(Some(name)) if name == DEFAULT => None,
        Ok(Some(name)) if name == AUTO => {
            let resolved = auto_name(prefers_dark, prefers_more_contrast);
            match apply(catalogue, resolved, Source::Auto, &mut lines) {
                Ok(theme) => return Ok(Startup { theme, lines }),
                Err(e) => Some((name, Source::Auto, e)),
            }
        }
        Ok(Some(name)) => {
            let source = source_of(catalogue, &name);
            match apply(catalogue, &name, source, &mut lines) {
                Ok(theme) => return Ok(Startup { theme, lines }),
                Err(e) => Some((name, source, e)),
            }
        }
        Err(message) => Some((
            "-".to_owned(),
            Source::Shipped,
            ThemeError::Storage { message },
        )),
    };
    if let Some((name, source, error)) = failed {
        lines.push(Line::Warn(signal::fallback(&name, source, &error, DEFAULT)));
    }
    let theme = apply(catalogue, DEFAULT, Source::Shipped, &mut lines)?;
    Ok(Startup { theme, lines })
}

/// Load the stored custom themes into `catalogue`, logging each one refused.
/// Unreadable or oversized storage loads nothing.
pub fn load_custom(catalogue: &mut Catalogue, stored: Result<Option<String>, String>) -> Vec<Line> {
    let raw = match stored {
        Ok(None) => return Vec::new(),
        Ok(Some(raw)) => raw,
        Err(message) => return vec![storage_refusal(message)],
    };
    if raw.len() > MAX_CUSTOM_BYTES {
        return vec![storage_refusal(format!(
            "stored custom themes are {} bytes, over the {MAX_CUSTOM_BYTES} byte limit",
            raw.len()
        ))];
    }
    let docs: Vec<String> = match serde_json::from_str(&raw) {
        Ok(docs) => docs,
        Err(e) => {
            return vec![storage_refusal(format!(
                "stored custom themes are not a JSON array of strings: {e}"
            ))]
        }
    };

    let mut lines = Vec::new();
    let dropped = docs.len().saturating_sub(MAX_CUSTOM);
    for source in docs.into_iter().take(MAX_CUSTOM) {
        if let Err((name, error)) = load_one(catalogue, source) {
            lines.push(Line::Warn(signal::fallback(
                &name,
                Source::Local,
                &error,
                "-",
            )));
        }
    }
    if dropped > 0 {
        lines.push(storage_refusal(format!(
            "custom theme limit of {MAX_CUSTOM} reached; {dropped} not loaded"
        )));
    }
    lines
}

/// The JSON array of every custom theme's source, in catalogue order.
pub fn custom_json(catalogue: &Catalogue) -> String {
    let sources: Vec<&str> = catalogue
        .names()
        .filter(|name| !is_shipped(name))
        .filter_map(|name| catalogue.get_source(name))
        .collect();
    serde_json::Value::from(sources).to_string()
}

/// Store a theme file as a custom theme. The catalogue changes only when the
/// theme resolves and passes contrast.
pub fn save(catalogue: &mut Catalogue, source: String, from: Source) -> Result<Saved, Refusal> {
    let (parsed, _) = format::read(&source).map_err(|e| refuse("-", from, &e, e.to_string()))?;
    let name = parsed.name().map(str::to_owned).ok_or_else(|| {
        let e = ThemeError::MissingKey {
            path: "meta.name".to_owned(),
        };
        refuse("-", from, &e, e.to_string())
    })?;

    if is_shipped(&name) {
        let e = shipped_name(&name);
        return Err(refuse(&name, from, &e, e.to_string()));
    }
    if catalogue.get_source(&name).is_none() && custom_count(catalogue) >= MAX_CUSTOM {
        let message = format!("the catalogue already holds {MAX_CUSTOM} custom themes");
        let e = ThemeError::Storage {
            message: message.clone(),
        };
        let reason = format!("{message}; delete one before saving another");
        return Err(refuse(&name, from, &e, reason));
    }

    let mut next = catalogue.clone();
    next.insert(source)
        .map_err(|e| refuse(&name, from, &e, e.to_string()))?;
    let stored = custom_json(&next).len();
    if stored > MAX_CUSTOM_BYTES {
        let message = format!(
            "custom themes would take {stored} bytes, over the {MAX_CUSTOM_BYTES} byte limit"
        );
        let e = ThemeError::Storage {
            message: message.clone(),
        };
        return Err(refuse(
            &name,
            from,
            &e,
            format!("{message}; delete one first"),
        ));
    }
    let resolved = next
        .resolve(&name)
        .map_err(|e| refuse(&name, from, &e, e.to_string()))?;

    let failures = contrast::failures(&resolved.theme);
    if !failures.is_empty() {
        let pairs: Vec<String> = failures
            .iter()
            .map(|r| format!("{} is {:.2}, needs {}", r.pair, r.ratio, r.min))
            .collect();
        return Err(Refusal {
            lines: failures
                .iter()
                .map(|r| Line::Warn(signal::rejected(&name, from, r)))
                .collect(),
            reason: format!("contrast is too low: {}", pairs.join("; ")),
        });
    }

    let mut lines: Vec<Line> = resolved
        .warnings
        .iter()
        .map(|w| Line::Warn(signal::warning(&name, from, w)))
        .collect();
    lines.push(Line::Info(signal::saved(
        &name,
        from,
        resolved.warnings.len(),
    )));
    *catalogue = next;
    Ok(Saved { name, lines })
}

fn apply(
    catalogue: &Catalogue,
    name: &str,
    source: Source,
    lines: &mut Vec<Line>,
) -> Result<Theme, ThemeError> {
    if catalogue.get_source(name).is_none() {
        return Err(ThemeError::UnknownTheme {
            name: name.to_owned(),
        });
    }
    let resolved = catalogue.resolve(name)?;
    for warning in &resolved.warnings {
        lines.push(Line::Warn(signal::warning(name, source, warning)));
    }
    lines.push(Line::Info(signal::applied(
        name,
        source,
        doc::SCHEMA,
        resolved.warnings.len(),
    )));
    Ok(resolved.theme)
}

/// Shipped for a shipped or unknown name, local for a custom one in the catalogue.
fn source_of(catalogue: &Catalogue, name: &str) -> Source {
    if is_shipped(name) || catalogue.get_source(name).is_none() {
        Source::Shipped
    } else {
        Source::Local
    }
}

fn load_one(catalogue: &mut Catalogue, source: String) -> Result<(), (String, ThemeError)> {
    let name = match format::read(&source) {
        Ok((parsed, _)) => parsed.name().map(str::to_owned),
        Err(e) => return Err(("-".to_owned(), e)),
    };
    let label = name.clone().unwrap_or_else(|| "-".to_owned());
    if let Some(shipped) = name.as_deref().filter(|n| is_shipped(n)) {
        return Err((label, shipped_name(shipped)));
    }
    catalogue.insert(source).map(|_| ()).map_err(|e| (label, e))
}

fn shipped_name(name: &str) -> ThemeError {
    ThemeError::BadValue {
        path: "meta.name".to_owned(),
        value: name.to_owned(),
        expected: "a name no shipped theme uses".to_owned(),
    }
}

fn custom_count(catalogue: &Catalogue) -> usize {
    catalogue.names().filter(|name| !is_shipped(name)).count()
}

fn storage_refusal(message: String) -> Line {
    Line::Warn(signal::fallback(
        "-",
        Source::Local,
        &ThemeError::Storage { message },
        "-",
    ))
}

fn refuse(name: &str, from: Source, error: &ThemeError, reason: String) -> Refusal {
    Refusal {
        lines: vec![Line::Warn(signal::fallback(name, from, error, "-"))],
        reason,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::theme::shipped;

    fn run(stored: Result<Option<String>, String>) -> Startup {
        startup(&shipped(), stored, false, false).unwrap()
    }

    fn custom(name: &str) -> String {
        format!("[meta]\nname = \"{name}\"\nschema = 1\n")
    }

    #[test]
    fn a_save_that_would_overflow_storage_is_refused_and_nothing_changes() {
        let padding = "# padding\n".repeat(60 * 1024 / 10);
        let mut catalogue = shipped();
        let mut saved = 0;
        let refusal = loop {
            let doc = format!("{}{padding}", custom(&format!("Big{saved}")));
            match save(&mut catalogue, doc, Source::Import) {
                Ok(_) => saved += 1,
                Err(refusal) => break refusal,
            }
            assert!(saved <= MAX_CUSTOM, "never refused");
        };
        assert!(refusal.reason.contains("byte limit"), "{}", refusal.reason);
        assert!(custom_json(&catalogue).len() <= MAX_CUSTOM_BYTES);
        assert!(catalogue.get_source(&format!("Big{saved}")).is_none());
    }

    fn json(docs: &[String]) -> String {
        serde_json::Value::from(docs.to_vec()).to_string()
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
            startup(&Catalogue::default(), Ok(None), false, false),
            Err(ThemeError::UnknownTheme { .. })
        ));
    }

    #[test]
    fn auto_follows_the_system_preferences() {
        let cases = [
            (false, false, "Daylight"),
            (true, false, "Pharos Night"),
            (false, true, "High Contrast"),
            (true, true, "High Contrast"),
        ];
        for (dark, contrast, expected) in cases {
            let s = startup(&shipped(), Ok(Some(AUTO.into())), dark, contrast).unwrap();
            assert_eq!(s.theme.name, expected, "dark={dark} contrast={contrast}");
        }
    }

    #[test]
    fn auto_says_it_is_auto_and_names_the_theme() {
        let s = startup(&shipped(), Ok(Some(AUTO.into())), false, false).unwrap();
        assert_eq!(
            s.lines,
            [Line::Info(
                "pharos: theme applied name=Daylight source=auto schema=1 warnings=0".into()
            )]
        );
    }

    #[test]
    fn auto_with_its_theme_missing_falls_back_naming_auto() {
        let mut catalogue = shipped();
        catalogue.remove("Daylight");
        let s = startup(&catalogue, Ok(Some(AUTO.into())), false, false).unwrap();
        assert_eq!(s.theme.name, DEFAULT);
        assert_eq!(
            s.lines.first(),
            Some(&Line::Warn(
                "pharos: theme fallback name=auto source=auto reason=unknown_theme path=- value=Daylight to=\"Pharos Night\""
                    .into()
            ))
        );
    }

    #[test]
    fn a_stored_custom_theme_is_applied_as_local() {
        let mut catalogue = shipped();
        catalogue.insert(custom("Mine")).unwrap();
        let s = startup(&catalogue, Ok(Some("Mine".into())), false, false).unwrap();
        assert_eq!(s.theme.name, "Mine");
        assert_eq!(
            s.lines,
            [Line::Info(
                "pharos: theme applied name=Mine source=local schema=1 warnings=0".into()
            )]
        );
    }

    #[test]
    fn a_stored_list_loads_each_custom_theme_silently() {
        let docs = [custom("Mine"), custom("Yours")];
        let mut catalogue = shipped();
        let lines = load_custom(&mut catalogue, Ok(Some(json(&docs))));
        assert_eq!(lines, []);
        assert_eq!(custom_json(&catalogue), json(&docs));
    }

    #[test]
    fn no_stored_list_loads_nothing_and_logs_nothing() {
        let mut catalogue = shipped();
        assert_eq!(load_custom(&mut catalogue, Ok(None)), []);
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn unreadable_custom_storage_logs_one_storage_line() {
        let mut catalogue = shipped();
        let lines = load_custom(&mut catalogue, Err("SecurityError: access denied".into()));
        assert_eq!(
            lines,
            [Line::Warn(
                "pharos: theme fallback name=- source=local reason=storage path=- value=\"SecurityError: access denied\" to=-"
                    .into()
            )]
        );
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn bad_json_loads_nothing_and_logs_one_storage_line() {
        let mut catalogue = shipped();
        let lines = load_custom(&mut catalogue, Ok(Some("[\"x\"".into())));
        assert_eq!(lines.len(), 1);
        assert!(matches!(
            lines.first(),
            Some(Line::Warn(l)) if l.starts_with("pharos: theme fallback name=- source=local reason=storage path=- value=\"stored custom themes are not a JSON array")
        ));
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn storage_over_the_byte_limit_is_refused_before_parsing() {
        let mut catalogue = shipped();
        let size = MAX_CUSTOM_BYTES + 1;
        let lines = load_custom(&mut catalogue, Ok(Some(" ".repeat(size))));
        assert_eq!(
            lines,
            [Line::Warn(format!(
                "pharos: theme fallback name=- source=local reason=storage path=- value=\"stored custom themes are {size} bytes, over the {MAX_CUSTOM_BYTES} byte limit\" to=-"
            ))]
        );
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn a_custom_theme_named_as_a_shipped_one_is_refused_and_the_shipped_kept() {
        let shipped_source = shipped().get_source("Kali Dark").unwrap().to_owned();
        let mut catalogue = shipped();
        let lines = load_custom(&mut catalogue, Ok(Some(json(&[custom("Kali Dark")]))));
        assert_eq!(
            lines,
            [Line::Warn(
                "pharos: theme fallback name=\"Kali Dark\" source=local reason=bad_value path=meta.name value=\"Kali Dark\" to=-"
                    .into()
            )]
        );
        assert_eq!(
            catalogue.get_source("Kali Dark"),
            Some(shipped_source.as_str())
        );
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn a_bad_document_is_skipped_and_the_rest_load() {
        let mut catalogue = shipped();
        let docs = ["[meta\n".to_owned(), custom("Mine")];
        let lines = load_custom(&mut catalogue, Ok(Some(json(&docs))));
        assert_eq!(lines.len(), 1);
        assert!(matches!(
            lines.first(),
            Some(Line::Warn(l)) if l.starts_with("pharos: theme fallback name=- source=local reason=syntax path=- value=")
        ));
        assert_eq!(custom_json(&catalogue), json(&[custom("Mine")]));
    }

    #[test]
    fn the_first_32_load_and_the_rest_log_one_line() {
        let docs: Vec<String> = (0..=MAX_CUSTOM).map(|i| custom(&format!("T{i}"))).collect();
        let mut catalogue = shipped();
        let lines = load_custom(&mut catalogue, Ok(Some(json(&docs))));
        assert_eq!(
            lines,
            [Line::Warn(format!(
                "pharos: theme fallback name=- source=local reason=storage path=- value=\"custom theme limit of {MAX_CUSTOM} reached; 1 not loaded\" to=-"
            ))]
        );
        assert_eq!(custom_json(&catalogue), json(&docs[..MAX_CUSTOM]));
    }

    #[test]
    fn custom_json_lists_only_custom_themes_in_catalogue_order() {
        let mut catalogue = shipped();
        catalogue.insert(custom("Yours")).unwrap();
        catalogue.insert(custom("Mine")).unwrap();
        assert_eq!(
            custom_json(&catalogue),
            json(&[custom("Yours"), custom("Mine")])
        );
    }

    #[test]
    fn a_valid_save_is_stored_and_logged() {
        let mut catalogue = shipped();
        let saved = save(&mut catalogue, custom("Mine"), Source::Editor).unwrap();
        assert_eq!(saved.name, "Mine");
        assert_eq!(
            saved.lines,
            [Line::Info(
                "pharos: theme saved name=Mine source=editor schema=1 warnings=0".into()
            )]
        );
        assert_eq!(custom_json(&catalogue), json(&[custom("Mine")]));
    }

    #[test]
    fn a_shipped_name_is_refused_and_nothing_changes() {
        let mut catalogue = shipped();
        let refusal = save(&mut catalogue, custom("Kali Dark"), Source::Import).unwrap_err();
        assert_eq!(
            refusal.reason,
            "meta.name: bad value \"Kali Dark\", expected a name no shipped theme uses"
        );
        assert_eq!(
            refusal.lines,
            [Line::Warn(
                "pharos: theme fallback name=\"Kali Dark\" source=import reason=bad_value path=meta.name value=\"Kali Dark\" to=-"
                    .into()
            )]
        );
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn a_contrast_failure_is_refused_with_a_rejected_line_per_pair() {
        let mut catalogue = shipped();
        let dim =
            "[meta]\nname = \"Dim\"\nschema = 1\n[palette]\nbg = \"#000000\"\nfg = \"#222222\"\n";
        let refusal = save(&mut catalogue, dim.into(), Source::Editor).unwrap_err();
        assert!(refusal.reason.starts_with("contrast is too low: fg/bg is "));
        assert!(refusal.lines.iter().all(|l| matches!(
            l,
            Line::Warn(s) if s.starts_with("pharos: theme rejected name=Dim source=editor reason=contrast pair=")
        )));
        assert!(refusal
            .lines
            .iter()
            .any(|l| matches!(l, Line::Warn(s) if s.contains(" pair=fg/bg ratio="))));
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn an_invalid_colour_is_refused_naming_its_path() {
        let mut catalogue = shipped();
        let bad = "[meta]\nname = \"Bad\"\nschema = 1\n[palette]\naccent = \"zz\"\n";
        let refusal = save(&mut catalogue, bad.into(), Source::Editor).unwrap_err();
        assert!(refusal.reason.contains("palette.accent"));
        assert_eq!(
            refusal.lines,
            [Line::Warn(
                "pharos: theme fallback name=Bad source=editor reason=bad_value path=palette.accent value=zz to=-"
                    .into()
            )]
        );
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn an_unreadable_source_is_refused_with_no_name() {
        let mut catalogue = shipped();
        let refusal = save(&mut catalogue, "[meta".into(), Source::Editor).unwrap_err();
        assert!(matches!(
            refusal.lines.first(),
            Some(Line::Warn(l)) if l.starts_with("pharos: theme fallback name=- source=editor reason=syntax path=- value=")
        ));
        assert_eq!(custom_json(&catalogue), "[]");
    }

    #[test]
    fn the_33rd_custom_theme_is_refused_and_a_resave_is_allowed() {
        let docs: Vec<String> = (0..MAX_CUSTOM).map(|i| custom(&format!("T{i}"))).collect();
        let mut catalogue = shipped();
        assert_eq!(load_custom(&mut catalogue, Ok(Some(json(&docs)))), []);

        let refusal = save(&mut catalogue, custom("T32"), Source::Editor).unwrap_err();
        assert_eq!(
            refusal.reason,
            "the catalogue already holds 32 custom themes; delete one before saving another"
        );
        assert!(matches!(
            refusal.lines.first(),
            Some(Line::Warn(l)) if l.starts_with("pharos: theme fallback name=T32 source=editor reason=storage")
        ));

        let edited = format!("{}[shape]\nradius_px = 3\n", custom("T0"));
        assert!(save(&mut catalogue, edited, Source::Editor).is_ok());
    }
}
