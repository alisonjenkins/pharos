//! The theme file format: the one module that knows theme files are TOML.
//! `read` checks that each key has the right type and names the keys it does
//! not know; `write` is its inverse. What the values mean is checked when a
//! theme is resolved.

use serde::Deserialize;
use toml::{Table, Value};

use super::doc::{clip, ThemeDoc, MAX_BYTES, NAME_MAX_CHARS, SCHEMA};
use super::resolve::{Kind, PaletteRole, ThemeError, ThemeWarning};

#[derive(Clone, Copy)]
enum Node {
    Str,
    Int,
    UInt,
    Float,
    Bool,
    StrList,
    Table(&'static [(&'static str, Node)]),
    Palette,
    Kinds,
}

const ROOT: &[(&str, Node)] = &[
    (
        "meta",
        Node::Table(&[
            ("name", Node::Str),
            ("schema", Node::UInt),
            ("author", Node::Str),
            ("version", Node::Str),
            ("scheme", Node::Str),
            ("extends", Node::Str),
        ]),
    ),
    ("palette", Node::Palette),
    (
        "type",
        Node::Table(&[
            ("ui", Node::Str),
            ("display", Node::Str),
            ("scale", Node::Float),
            ("base_px", Node::Int),
            ("mono_ui", Node::Bool),
        ]),
    ),
    (
        "shape",
        Node::Table(&[
            ("radius_px", Node::Int),
            ("border_px", Node::Int),
            ("density", Node::Str),
        ]),
    ),
    (
        "motion",
        Node::Table(&[
            ("duration_ms", Node::Int),
            ("easing", Node::Str),
            ("focus_scale", Node::Float),
        ]),
    ),
    (
        "effects",
        Node::Table(&[
            ("background", Node::Str),
            ("shader", Node::Str),
            ("blur_px", Node::Int),
            ("vignette", Node::Float),
            ("grain", Node::Float),
            ("glow", Node::Float),
        ]),
    ),
    (
        "layout",
        Node::Table(&[
            ("hero", Node::Bool),
            ("rail_poster", Node::Str),
            ("rail_collapsed", Node::Bool),
        ]),
    ),
    (
        "contrast",
        Node::Table(&[("level", Node::Str), ("decorative", Node::StrList)]),
    ),
];

fn child(node: Node, key: &str) -> Option<Node> {
    match node {
        Node::Table(entries) => entries
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, node)| *node),
        Node::Palette if key == "kind" => Some(Node::Kinds),
        Node::Palette => PaletteRole::parse(key).map(|_| Node::Str),
        Node::Kinds => Kind::parse(key).map(|_| Node::Str),
        _ => None,
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn show(value: &Value) -> String {
    match value {
        Value::String(s) => clip(s),
        other => clip(&other.to_string()),
    }
}

fn mismatch(path: &str, value: &Value, expected: &str) -> ThemeError {
    ThemeError::BadValue {
        path: path.to_owned(),
        value: show(value),
        expected: expected.to_owned(),
    }
}

fn check_value(
    node: Node,
    path: &str,
    value: &Value,
    warnings: &mut Vec<ThemeWarning>,
) -> Result<(), ThemeError> {
    let ok = match node {
        Node::Str => value.is_str(),
        Node::Int => value.is_integer(),
        Node::UInt => value.as_integer().is_some_and(|n| u32::try_from(n).is_ok()),
        Node::Float => value.is_float() || value.is_integer(),
        Node::Bool => value.is_bool(),
        Node::StrList => {
            let Some(items) = value.as_array() else {
                return Err(mismatch(path, value, "an array of strings"));
            };
            for (i, item) in items.iter().enumerate() {
                if !item.is_str() {
                    return Err(mismatch(&format!("{path}[{i}]"), item, "a string"));
                }
            }
            true
        }
        Node::Table(_) | Node::Palette | Node::Kinds => {
            let Some(table) = value.as_table() else {
                return Err(mismatch(path, value, "a table"));
            };
            return walk(table, node, path, warnings);
        }
    };
    if ok {
        return Ok(());
    }
    let expected = match node {
        Node::Str => "a string",
        Node::Int => "an integer",
        Node::UInt => "a non-negative integer",
        Node::Float => "a number",
        _ => "a boolean",
    };
    Err(mismatch(path, value, expected))
}

fn walk(
    table: &Table,
    node: Node,
    prefix: &str,
    warnings: &mut Vec<ThemeWarning>,
) -> Result<(), ThemeError> {
    for (key, value) in table {
        let path = join(prefix, key);
        match child(node, key) {
            Some(known) => check_value(known, &path, value, warnings)?,
            None => warnings.push(ThemeWarning::UnknownKey { path }),
        }
    }
    Ok(())
}

fn from_table(table: Table) -> Result<ThemeDoc, ThemeError> {
    ThemeDoc::deserialize(table).map_err(|e| ThemeError::BadValue {
        path: "(document)".to_owned(),
        value: clip(&e.to_string()),
        expected: "a schema 1 theme document".to_owned(),
    })
}

fn missing(path: &str) -> ThemeError {
    ThemeError::MissingKey {
        path: path.to_owned(),
    }
}

/// Read a theme file. An unknown key or a newer schema loads with a warning;
/// anything else wrong rejects the file.
pub fn read(src: &str) -> Result<(ThemeDoc, Vec<ThemeWarning>), ThemeError> {
    if src.len() > MAX_BYTES {
        return Err(ThemeError::TooLarge { bytes: src.len() });
    }
    let table: Table = src
        .parse()
        .map_err(|e: toml::de::Error| ThemeError::Syntax {
            message: e.to_string(),
        })?;

    let mut warnings = Vec::new();
    walk(&table, Node::Table(ROOT), "", &mut warnings)?;

    let doc = from_table(table)?;

    let meta = doc.meta.as_ref();
    let name = meta
        .and_then(|m| m.name.as_deref())
        .ok_or_else(|| missing("meta.name"))?;
    let schema = meta
        .and_then(|m| m.schema)
        .ok_or_else(|| missing("meta.schema"))?;

    if !(1..=NAME_MAX_CHARS).contains(&name.trim().chars().count()) {
        return Err(ThemeError::BadValue {
            path: "meta.name".to_owned(),
            value: clip(name),
            expected: format!("1 to {NAME_MAX_CHARS} characters"),
        });
    }
    if schema == 0 {
        return Err(ThemeError::BadValue {
            path: "meta.schema".to_owned(),
            value: schema.to_string(),
            expected: "an integer of 1 or more".to_owned(),
        });
    }
    if schema > SCHEMA {
        warnings.push(ThemeWarning::NewerSchema {
            found: schema,
            supported: SCHEMA,
        });
    }
    Ok((doc, warnings))
}

/// Serialise `doc` as a theme file: sections in the documented order, absent
/// fields left out.
pub fn write(doc: &ThemeDoc) -> Result<String, ThemeError> {
    toml::to_string(doc).map_err(|e| ThemeError::Syntax {
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const MINIMAL: &str = "[meta]\nname = \"T\"\nschema = 1\n";

    fn read_ok(src: &str) -> (ThemeDoc, Vec<ThemeWarning>) {
        read(src).unwrap()
    }

    #[test]
    fn a_document_with_only_name_and_schema_loads() {
        let (doc, warnings) = read_ok(MINIMAL);
        assert_eq!(doc.name(), Some("T"));
        assert_eq!(warnings, vec![]);
    }

    #[test]
    fn unknown_keys_at_any_depth_warn_with_their_full_path() {
        let src = format!("{MINIMAL}[palette]\nglitter = \"#fff\"\n[nope]\nx = 1\n[palette.kind]\ntv = \"#fff\"\n");
        let (doc, warnings) = read_ok(&src);
        assert_eq!(doc.name(), Some("T"));
        assert_eq!(
            warnings,
            vec![
                ThemeWarning::UnknownKey {
                    path: "nope".into()
                },
                ThemeWarning::UnknownKey {
                    path: "palette.glitter".into()
                },
                ThemeWarning::UnknownKey {
                    path: "palette.kind.tv".into()
                },
            ]
        );
    }

    #[test]
    fn a_newer_schema_loads_with_a_warning() {
        let (_, warnings) = read_ok("[meta]\nname = \"T\"\nschema = 2\n");
        assert_eq!(
            warnings,
            vec![ThemeWarning::NewerSchema {
                found: 2,
                supported: 1
            }]
        );
    }

    #[test]
    fn schema_zero_is_a_bad_value() {
        let err = read("[meta]\nname = \"T\"\nschema = 0\n").unwrap_err();
        assert!(
            matches!(&err, ThemeError::BadValue { path, value, .. } if path == "meta.schema" && value == "0"),
            "{err:?}"
        );
    }

    #[test]
    fn a_missing_name_or_schema_is_named() {
        assert_eq!(
            read("[meta]\nschema = 1\n").unwrap_err(),
            ThemeError::MissingKey {
                path: "meta.name".into()
            }
        );
        assert_eq!(
            read("[meta]\nname = \"T\"\n").unwrap_err(),
            ThemeError::MissingKey {
                path: "meta.schema".into()
            }
        );
        assert_eq!(
            read("").unwrap_err(),
            ThemeError::MissingKey {
                path: "meta.name".into()
            }
        );
    }

    #[test]
    fn a_name_must_be_one_to_sixty_four_characters_after_trimming() {
        for name in ["", "   ", &"x".repeat(65)] {
            let err = read(&format!("[meta]\nname = \"{name}\"\nschema = 1\n")).unwrap_err();
            assert!(
                matches!(&err, ThemeError::BadValue { path, .. } if path == "meta.name"),
                "{err:?}"
            );
        }
        read_ok(&format!(
            "[meta]\nname = \"{}\"\nschema = 1\n",
            "x".repeat(64)
        ));
    }

    #[test]
    fn a_wrongly_typed_value_names_its_path_and_value() {
        let cases = [
            ("[palette]\naccent = 5\n", "palette.accent", "5"),
            ("[shape]\nradius_px = 1.5\n", "shape.radius_px", "1.5"),
            ("[type]\nmono_ui = \"yes\"\n", "type.mono_ui", "yes"),
            (
                "[contrast]\ndecorative = [\"badge\", 3]\n",
                "contrast.decorative[1]",
                "3",
            ),
            ("[palette]\nkind = \"x\"\n", "palette.kind", "x"),
        ];
        for (extra, path, value) in cases {
            let err = read(&format!("{MINIMAL}{extra}")).unwrap_err();
            assert!(
                matches!(&err, ThemeError::BadValue { path: p, value: v, .. } if p == path && v == value),
                "{extra}: {err:?}"
            );
        }
    }

    #[test]
    fn a_syntax_error_carries_the_line() {
        let err = read("[meta\nname = 1").unwrap_err();
        let ThemeError::Syntax { message } = err else {
            panic!("expected Syntax, got {err:?}");
        };
        assert!(message.contains("line 1"), "{message}");
    }

    #[test]
    fn a_file_over_the_size_limit_is_rejected_before_parsing() {
        let src = " ".repeat(MAX_BYTES + 1);
        assert_eq!(
            read(&src).unwrap_err(),
            ThemeError::TooLarge {
                bytes: MAX_BYTES + 1
            }
        );
        assert!(!matches!(
            read(&" ".repeat(MAX_BYTES)),
            Err(ThemeError::TooLarge { .. })
        ));
    }

    #[test]
    fn every_palette_role_and_kind_is_readable_under_its_label() {
        let mut src = String::from(MINIMAL);
        src.push_str("[palette]\n");
        for role in PaletteRole::ALL {
            src.push_str(&format!("{} = \"#123456\"\n", role.label()));
        }
        src.push_str("[palette.kind]\n");
        for kind in Kind::ALL {
            src.push_str(&format!("{} = \"#654321\"\n", kind.label()));
        }
        let (doc, warnings) = read_ok(&src);
        assert_eq!(warnings, vec![]);
        let palette = doc.palette.unwrap();
        for role in PaletteRole::ALL {
            assert_eq!(
                palette.get(*role).map(String::as_str),
                Some("#123456"),
                "{role:?}"
            );
        }
        let kinds = palette.kind.unwrap();
        for kind in Kind::ALL {
            assert_eq!(
                kinds.get(*kind).map(String::as_str),
                Some("#654321"),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_document_serialises_back_to_the_keys_it_had() {
        let src = format!("{MINIMAL}[palette]\nfg-muted = \"#111\"\n[type]\nscale = 1.2\n");
        let (doc, _) = read_ok(&src);
        let table = Table::try_from(&doc).unwrap();
        let expected: Table = src.parse().unwrap();
        assert_eq!(table, expected);
    }

    #[test]
    fn every_shipped_theme_survives_write_then_read() {
        for source in crate::theme::SHIPPED {
            let (doc, _) = read_ok(source);
            let text = write(&doc).unwrap();
            let (again, warnings) = read(&text).unwrap();
            assert_eq!(again, doc, "{:?}\n{text}", doc.name());
            assert_eq!(warnings, vec![], "{:?}", doc.name());
        }
    }

    #[test]
    fn writing_omits_absent_sections_and_fields() {
        let src = format!("{MINIMAL}[palette]\naccent = \"#123456\"\n");
        let (doc, _) = read_ok(&src);
        let text = write(&doc).unwrap();
        assert!(!text.contains("[shape]"), "{text}");
        assert!(!text.contains("bg"), "{text}");
        let (again, _) = read(&text).unwrap();
        assert_eq!(again, doc);
    }

    #[test]
    fn written_sections_follow_the_documented_order() {
        let (doc, _) = read_ok(crate::theme::SHIPPED[0]);
        let text = write(&doc).unwrap();
        let headers = [
            "[meta]",
            "[palette]",
            "[palette.kind]",
            "[type]",
            "[shape]",
            "[motion]",
            "[effects]",
            "[layout]",
            "[contrast]",
        ];
        let at: Vec<_> = headers.iter().map(|h| text.find(h)).collect();
        assert!(at.iter().all(Option::is_some), "{text}");
        assert!(at.is_sorted(), "{at:?}\n{text}");
    }

    #[test]
    fn only_this_module_names_the_file_format() {
        let others = [
            ("contrast.rs", include_str!("contrast.rs")),
            ("css.rs", include_str!("css.rs")),
            ("device.rs", include_str!("device.rs")),
            ("doc.rs", include_str!("doc.rs")),
            ("editor.rs", include_str!("editor.rs")),
            ("mod.rs", include_str!("mod.rs")),
            ("resolve.rs", include_str!("resolve.rs")),
            ("signal.rs", include_str!("signal.rs")),
            ("value.rs", include_str!("value.rs")),
        ];
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/theme");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".rs") && n != "format.rs")
            .collect();
        on_disk.sort();
        let listed: Vec<&str> = others.iter().map(|(file, _)| *file).collect();
        assert_eq!(on_disk, listed, "add the new theme module to this list");
        for (file, text) in others {
            assert!(
                !text.contains("toml::") && !text.contains("use toml"),
                "{file} names the file format; only format.rs may"
            );
        }
    }
}
