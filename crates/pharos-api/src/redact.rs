//! Decoded, redacted rendering of RPC params and results for debug logs, spans
//! and (later) `pharos rpc tap` (017 FR-011, V8). Fields and params annotated
//! `$secret` in the schema render as `<redacted>`. The check reads the
//! annotation through capnp's runtime reflection, so a new `$secret` field is
//! covered without touching this file.

use std::fmt::Write as _;

use capnp::dynamic_value;
use capnp::schema::Field;

use crate::server::SECRET_ANNOTATION_ID;

pub const REDACTED: &str = "<redacted>";

/// Nesting beyond this renders as `…`; the schema nests a handful deep.
const MAX_DEPTH: usize = 8;
/// List elements rendered before the rest are summarised as a count.
const MAX_LIST: u32 = 16;
/// Text longer than this is cut, with its full length noted.
const MAX_TEXT: usize = 256;

/// Renders `value` as compact, schema-named text with secrets redacted.
pub fn render(value: dynamic_value::Reader<'_>) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out
}

/// Whether `field` carries the `$secret` annotation.
pub fn is_secret(field: Field) -> bool {
    field
        .get_annotations()
        .map(|a| a.find(SECRET_ANNOTATION_ID).is_some())
        .unwrap_or(false)
}

/// `t` cut to at most `max` bytes, with `…(<len> bytes)` appended when cut;
/// text that is not UTF-8 renders as `<invalid utf-8: <len> bytes>` rather
/// than vanishing.
pub(crate) fn bounded_text(t: capnp::text::Reader<'_>, max: usize) -> String {
    match t.to_str() {
        Ok(s) => format!("{}{}", cut(s, max), cut_note(s, max)),
        Err(_) => invalid_utf8(t),
    }
}

fn cut(s: &str, max: usize) -> &str {
    let end = (0..=max.min(s.len()))
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    &s[..end]
}

fn cut_note(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("…({} bytes)", s.len())
    } else {
        String::new()
    }
}

fn invalid_utf8(t: capnp::text::Reader<'_>) -> String {
    format!("<invalid utf-8: {} bytes>", t.as_bytes().len())
}

fn write_value(out: &mut String, value: dynamic_value::Reader<'_>, depth: usize) {
    use dynamic_value::Reader as V;
    if depth > MAX_DEPTH {
        out.push('…');
        return;
    }
    let _ = match value {
        V::Void => write!(out, "()"),
        V::Bool(v) => write!(out, "{v}"),
        V::Int8(v) => write!(out, "{v}"),
        V::Int16(v) => write!(out, "{v}"),
        V::Int32(v) => write!(out, "{v}"),
        V::Int64(v) => write!(out, "{v}"),
        V::UInt8(v) => write!(out, "{v}"),
        V::UInt16(v) => write!(out, "{v}"),
        V::UInt32(v) => write!(out, "{v}"),
        V::UInt64(v) => write!(out, "{v}"),
        V::Float32(v) => write!(out, "{v}"),
        V::Float64(v) => write!(out, "{v}"),
        V::Enum(e) => match e.get_enumerant() {
            Ok(Some(en)) => write!(out, "{}", name_of(en.get_proto().get_name())),
            _ => write!(out, "#{}", e.get_value()),
        },
        V::Text(t) => match t.to_str() {
            Ok(s) => write!(out, "{:?}{}", cut(s, MAX_TEXT), cut_note(s, MAX_TEXT)),
            Err(_) => write!(out, "{}", invalid_utf8(t)),
        },
        V::Data(d) => write!(out, "<{} bytes>", d.len()),
        V::Struct(s) => {
            write_struct(out, s, depth);
            Ok(())
        }
        V::List(l) => {
            out.push('[');
            let len = l.len();
            for i in 0..len.min(MAX_LIST) {
                if i > 0 {
                    out.push_str(", ");
                }
                match l.get(i) {
                    Ok(v) => write_value(out, v, depth + 1),
                    Err(e) => {
                        let _ = write!(out, "<{e}>");
                    }
                }
            }
            if len > MAX_LIST {
                let _ = write!(out, ", …(+{})", len - MAX_LIST);
            }
            out.push(']');
            Ok(())
        }
        V::AnyPointer(_) => write!(out, "<any>"),
        V::Capability(_) => write!(out, "<capability>"),
    };
}

fn write_struct(out: &mut String, s: capnp::dynamic_struct::Reader<'_>, depth: usize) {
    let schema = s.get_schema();
    out.push('{');
    let mut first = true;
    let mut field = |out: &mut String, f: Field| {
        // An unset pointer field is "absent", not empty (data-model.md).
        if !s.has(f).unwrap_or(false) {
            return;
        }
        if !first {
            out.push_str(", ");
        }
        first = false;
        let _ = write!(out, "{}: ", name_of(f.get_proto().get_name()));
        if is_secret(f) {
            out.push_str(REDACTED);
            return;
        }
        match s.get(f) {
            Ok(v) => write_value(out, v, depth + 1),
            Err(e) => {
                let _ = write!(out, "<{e}>");
            }
        }
    };
    if let Ok(fields) = schema.get_non_union_fields() {
        for f in fields {
            field(out, f);
        }
    }
    if let Ok(Some(active)) = s.which() {
        field(out, active);
    }
    out.push('}');
}

/// A schema node's name, or `?` when it cannot be read.
pub(crate) fn name_of(name: capnp::Result<capnp::text::Reader<'_>>) -> &str {
    name.ok().and_then(|n| n.to_str().ok()).unwrap_or("?")
}
