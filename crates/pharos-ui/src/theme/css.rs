//! A theme as CSS custom properties (contracts/css-tokens.md).

use super::resolve::{Kind, PaletteRole, Theme};
use super::value::Density;

const REM_PX: f64 = 16.0;
const TYPE_STEPS: [i32; 6] = [-1, 0, 1, 2, 3, 4];

/// `v` to four decimals with trailing zeros dropped, so `1.2000000000000002`
/// prints as `1.2`.
fn num(v: f64) -> String {
    let fixed = format!("{v:.4}");
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    match trimmed {
        "-0" | "" => "0".to_owned(),
        other => other.to_owned(),
    }
}

/// Every declaration of the `:root` block as name and value, in emission order.
pub fn declarations(theme: &Theme) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |name: String, value: String| out.push((name, value));

    push("color-scheme".to_owned(), theme.scheme.label().to_owned());
    for role in PaletteRole::ALL {
        push(
            format!("--px-color-{}", role.label()),
            theme.palette.get(*role).to_css(),
        );
    }
    for kind in Kind::ALL {
        push(
            format!("--px-color-kind-{}", kind.label()),
            theme.kind.get(*kind).to_css(),
        );
    }
    push("--px-font-ui".to_owned(), theme.ui.css().to_owned());
    push(
        "--px-font-display".to_owned(),
        theme.display.css().to_owned(),
    );
    push("--px-type-base".to_owned(), format!("{}px", theme.base_px));
    push("--px-type-scale".to_owned(), num(theme.scale));
    for step in TYPE_STEPS {
        let px = f64::from(theme.base_px) * theme.scale.powi(step);
        push(
            format!("--px-type-step-{step}"),
            format!("{}rem", num(px / REM_PX)),
        );
    }
    let space_unit = match theme.density {
        Density::Comfortable => 8,
        Density::Compact => 6,
    };
    push("--px-space-unit".to_owned(), format!("{space_unit}px"));
    push("--px-radius".to_owned(), format!("{}px", theme.radius_px));
    push("--px-border".to_owned(), format!("{}px", theme.border_px));
    push(
        "--px-motion-duration".to_owned(),
        format!("{}ms", theme.duration_ms),
    );
    push(
        "--px-motion-easing".to_owned(),
        theme.easing.css().to_owned(),
    );
    push("--px-motion-focus-scale".to_owned(), num(theme.focus_scale));
    push(
        "--px-effect-blur".to_owned(),
        format!("{}px", theme.effects.blur_px),
    );
    push(
        "--px-effect-vignette".to_owned(),
        num(theme.effects.vignette),
    );
    push("--px-effect-glow".to_owned(), num(theme.effects.glow));
    out
}

/// The theme as one `:root { … }` block.
pub fn emit(theme: &Theme) -> String {
    let mut css = String::from(":root {\n");
    for (name, value) in declarations(theme) {
        css.push_str(&format!("  {name}: {value};\n"));
    }
    css.push_str("}\n");
    css
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::theme::{shipped, DEFAULT};

    const TOKEN_NAMES: &[&str] = &[
        "--px-color-bg",
        "--px-color-surface",
        "--px-color-surface-raised",
        "--px-color-overlay",
        "--px-color-border",
        "--px-color-fg",
        "--px-color-fg-muted",
        "--px-color-accent",
        "--px-color-accent-fg",
        "--px-color-focus",
        "--px-color-success",
        "--px-color-warning",
        "--px-color-danger",
        "--px-color-progress",
        "--px-color-badge",
        "--px-color-kind-movie",
        "--px-color-kind-show",
        "--px-color-kind-music",
        "--px-color-kind-book",
        "--px-color-kind-live",
        "--px-font-ui",
        "--px-font-display",
        "--px-type-base",
        "--px-type-scale",
        "--px-type-step--1",
        "--px-type-step-0",
        "--px-type-step-1",
        "--px-type-step-2",
        "--px-type-step-3",
        "--px-type-step-4",
        "--px-space-unit",
        "--px-radius",
        "--px-border",
        "--px-motion-duration",
        "--px-motion-easing",
        "--px-motion-focus-scale",
        "--px-effect-blur",
        "--px-effect-vignette",
        "--px-effect-glow",
    ];

    fn night() -> Theme {
        shipped().resolve(DEFAULT).unwrap().theme
    }

    fn value_of(css: &str, name: &str) -> String {
        let prefix = format!("  {name}: ");
        css.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .and_then(|v| v.strip_suffix(';'))
            .unwrap()
            .to_owned()
    }

    #[test]
    fn the_block_opens_with_the_colour_scheme_and_first_colour() {
        let css = emit(&night());
        assert!(
            css.starts_with(":root {\n  color-scheme: dark;\n  --px-color-bg: #0e1116;\n"),
            "{css}"
        );
        assert!(css.ends_with("}\n"));
        assert_eq!(css.matches(":root").count(), 1);
    }

    #[test]
    fn every_contract_token_is_emitted_exactly_once() {
        let css = emit(&night());
        for name in TOKEN_NAMES {
            assert_eq!(css.matches(&format!("  {name}: ")).count(), 1, "{name}");
        }
        assert_eq!(declarations(&night()).len(), TOKEN_NAMES.len() + 1);
    }

    #[test]
    fn no_value_carries_float_noise() {
        for theme in shipped()
            .names()
            .map(|n| shipped().resolve(n).unwrap().theme)
        {
            let css = emit(&theme);
            assert!(!css.contains("000000000"), "{}: {css}", theme.name);
        }
    }

    #[test]
    fn type_steps_follow_base_times_scale_to_the_n_in_rem() {
        let css = emit(&night());
        assert_eq!(value_of(&css, "--px-type-base"), "16px");
        assert_eq!(value_of(&css, "--px-type-scale"), "1.2");
        assert_eq!(value_of(&css, "--px-type-step--1"), "0.8333rem");
        assert_eq!(value_of(&css, "--px-type-step-0"), "1rem");
        assert_eq!(value_of(&css, "--px-type-step-1"), "1.2rem");
        assert_eq!(value_of(&css, "--px-type-step-4"), "2.0736rem");
    }

    #[test]
    fn units_and_lists_follow_the_contract() {
        let css = emit(&night());
        assert_eq!(value_of(&css, "--px-space-unit"), "8px");
        assert_eq!(value_of(&css, "--px-radius"), "8px");
        assert_eq!(value_of(&css, "--px-border"), "1px");
        assert_eq!(value_of(&css, "--px-motion-duration"), "180ms");
        assert_eq!(
            value_of(&css, "--px-motion-easing"),
            "cubic-bezier(0.2, 0, 0, 1)"
        );
        assert_eq!(value_of(&css, "--px-motion-focus-scale"), "1.05");
        assert_eq!(value_of(&css, "--px-effect-blur"), "24px");
        assert_eq!(
            value_of(&css, "--px-font-ui"),
            "system-ui, -apple-system, \"Segoe UI\", sans-serif"
        );
    }

    #[test]
    fn compact_density_and_light_scheme_show_in_the_block() {
        let mut theme = night();
        theme.density = Density::Compact;
        theme.scheme = crate::theme::value::Scheme::Light;
        let css = emit(&theme);
        assert_eq!(value_of(&css, "--px-space-unit"), "6px");
        assert!(css.starts_with(":root {\n  color-scheme: light;\n"));
    }

    #[test]
    fn emit_is_built_from_declarations() {
        let theme = night();
        let rebuilt: String = declarations(&theme)
            .iter()
            .map(|(n, v)| format!("  {n}: {v};\n"))
            .collect();
        assert_eq!(emit(&theme), format!(":root {{\n{rebuilt}}}\n"));
    }

    #[test]
    fn numbers_round_to_four_decimals_and_trim() {
        assert_eq!(num(1.2000000000000002), "1.2");
        assert_eq!(num(1.0), "1");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(0.83333333), "0.8333");
        assert_eq!(num(-0.00001), "0");
    }
}
