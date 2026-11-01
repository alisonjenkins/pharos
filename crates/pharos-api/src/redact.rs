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
        V::Text(t) => {
            let s = t.to_string().unwrap_or_default();
            if s.len() > MAX_TEXT {
                let cut = (0..=MAX_TEXT)
                    .rev()
                    .find(|i| s.is_char_boundary(*i))
                    .unwrap_or(0);
                write!(out, "{:?}…({} bytes)", &s[..cut], s.len())
            } else {
                write!(out, "{s:?}")
            }
        }
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

fn name_of(name: capnp::Result<capnp::text::Reader<'_>>) -> &str {
    name.ok().and_then(|n| n.to_str().ok()).unwrap_or("?")
}
