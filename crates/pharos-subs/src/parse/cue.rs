//! SRT and WebVTT cues, converted to ASS-equivalent events.

use super::tags::parse_text;
use crate::model::{Event, Format, Script};

/// `hh:mm:ss,mmm`, `mm:ss.mmm` or `hh:mm:ss.mmm` to milliseconds.
fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (hms, frac) = s.split_once([',', '.']).unwrap_or((s, "0"));
    let mut parts = hms.split(':').rev();
    let sec: i64 = parts.next()?.trim().parse().ok()?;
    let min: i64 = parts.next()?.trim().parse().ok()?;
    let hour: i64 = match parts.next() {
        Some(h) => h.trim().parse().ok()?,
        None => 0,
    };
    let mut ms = 0i64;
    let mut scale = 100;
    for c in frac.chars().take(3) {
        ms += i64::from(c.to_digit(10)?) * scale;
        scale /= 10;
    }
    Some(((hour * 60 + min) * 60 + sec) * 1000 + ms)
}

/// Converts one cue's lines to the ASS override-text dialect: `<b>` becomes
/// `{\b1}`, line ends become `\N`, other tags are dropped. `keep_braces`
/// lets SRT's `{\an8}` through; VTT has no such syntax so braces are defanged.
fn markup_to_ass(lines: &[&str], keep_braces: bool) -> String {
    let mut out = String::new();
    for (n, line) in lines.iter().enumerate() {
        if n > 0 {
            out.push_str("\\N");
        }
        let mut rest = *line;
        while !rest.is_empty() {
            let Some(p) = rest.find(['<', '&', '{', '}', '\\']) else {
                out.push_str(rest);
                break;
            };
            out.push_str(rest.get(..p).unwrap_or(""));
            let tail = rest.get(p..).unwrap_or("");
            let c = tail.chars().next().unwrap_or(' ');
            match c {
                '<' => match tail.find('>') {
                    Some(e) => {
                        html_tag(tail.get(1..e).unwrap_or(""), &mut out);
                        rest = tail.get(e + 1..).unwrap_or("");
                        continue;
                    }
                    None => out.push('<'),
                },
                '&' => match entity(tail) {
                    Some((s, len)) => {
                        out.push_str(s);
                        rest = tail.get(len..).unwrap_or("");
                        continue;
                    }
                    None => out.push('&'),
                },
                '{' | '}' if keep_braces => out.push(c),
                '{' => out.push('('),
                '}' => out.push(')'),
                '\\' if keep_braces => out.push('\\'),
                // A literal backslash must not start an ASS escape.
                _ => out.push('\u{29f5}'),
            }
            rest = tail.get(c.len_utf8()..).unwrap_or("");
        }
    }
    out
}

fn entity(tail: &str) -> Option<(&'static str, usize)> {
    for (name, rep) in [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", "\\h"),
        ("&lrm;", "\u{200e}"),
        ("&rlm;", "\u{200f}"),
        ("&quot;", "\""),
    ] {
        if tail.starts_with(name) {
            return Some((rep, name.len()));
        }
    }
    None
}

fn html_tag(tag: &str, out: &mut String) {
    let tag = tag.trim();
    let (closing, name_part) = match tag.strip_prefix('/') {
        Some(t) => (true, t),
        None => (false, tag),
    };
    let name = name_part
        .split(|c: char| c.is_whitespace() || c == '.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let on = if closing { '0' } else { '1' };
    match name.as_str() {
        "b" => {
            out.push_str("{\\b");
            out.push(on);
            out.push('}');
        }
        "i" => {
            out.push_str("{\\i");
            out.push(on);
            out.push('}');
        }
        "u" => {
            out.push_str("{\\u");
            out.push(on);
            out.push('}');
        }
        "s" => {
            out.push_str("{\\s");
            out.push(on);
            out.push('}');
        }
        "font" if closing => out.push_str("{\\c}"),
        "font" => {
            if let Some(hex) = font_colour(name_part) {
                out.push_str("{\\c&H");
                out.push_str(&hex);
                out.push_str("&}");
            }
        }
        _ => {}
    }
}

/// `<font color="#rrggbb">` -> `bbggrr`.
fn font_colour(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find("color")?;
    let v = lower
        .get(at + 5..)?
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let v = v.trim_start_matches(['"', '\'']);
    let v = v.strip_prefix('#').unwrap_or(v);
    let hex = v.get(..6)?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (r, g, b) = (hex.get(0..2)?, hex.get(2..4)?, hex.get(4..6)?);
    Some(format!("{b}{g}{r}"))
}

fn push_cue(script: &mut Script, start: i64, end: i64, lines: &[&str], keep_braces: bool) {
    if end <= start || lines.is_empty() {
        return;
    }
    let raw = markup_to_ass(lines, keep_braces);
    let parsed = parse_text(&raw, script.wrap_style, &mut script.stats);
    if parsed.text.trim().is_empty() {
        return;
    }
    script.events.push(Event {
        start_ms: start,
        end_ms: end,
        layer: 0,
        style: 0,
        margin_l: 0,
        margin_r: 0,
        margin_v: 0,
        alignment: parsed.alignment,
        text: parsed.text,
        runs: parsed.runs,
    });
}

pub(crate) fn parse_srt(src: &str) -> Script {
    parse_blocks(src, Format::Srt)
}

pub(crate) fn parse_vtt(src: &str) -> Script {
    parse_blocks(src, Format::WebVtt)
}

fn parse_blocks(src: &str, format: Format) -> Script {
    let mut script = Script::empty(format);
    let vtt = format == Format::WebVtt;
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines.get(i).copied().unwrap_or("");
        let Some((a, b)) = line.split_once("-->") else {
            i += 1;
            continue;
        };
        // WebVTT cue settings trail the end time.
        let b = b.split_whitespace().next().unwrap_or("");
        let times = parse_time(a).zip(parse_time(b));
        let mut j = i + 1;
        while lines.get(j).is_some_and(|l| !l.trim().is_empty()) {
            j += 1;
        }
        match times {
            Some((s, e)) => {
                let body = lines.get(i + 1..j).unwrap_or(&[]);
                push_cue(&mut script, s, e, body, !vtt);
            }
            None => script.stats.bad_lines += 1,
        }
        i = j;
    }
    script.finish();
    script
}
