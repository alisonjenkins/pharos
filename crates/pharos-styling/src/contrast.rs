//! A contrast nudge for signs passed through from the source (B1210).
//!
//! Typesetters often colour a translated sign like the lettering it
//! translates, so the two merge on screen. A sign's fill is pushed a little
//! further from its outline: a light fill lighter and its outline darker, or
//! the reverse for dark text. Placement, size and font stay as authored.
//! Drawings are left alone: their fill is a shape, not text.

use crate::tags::{self, Piece};

/// Share of the way a fill moves towards white (or black).
const FILL_SHIFT: f32 = 0.25;
/// Share of the way an outline or shadow moves towards black (or white).
const EDGE_SHIFT: f32 = 0.6;

/// Whether a style's text is lighter than its outline, from its `Format:`
/// and `Style:` lines; light when either colour cannot be read.
pub(crate) fn style_is_light(format: &str, line: &str) -> bool {
    let col = |name: &str| field(format, line, name).and_then(|v| parse_colour(v.trim()));
    match (col("PrimaryColour"), col("OutlineColour")) {
        (Some(fill), Some(edge)) => luma(fill) >= luma(edge),
        _ => true,
    }
}

/// `line` with its fill, outline and shadow colours nudged apart; as it
/// is when its fill or outline cannot be read.
pub(crate) fn style_line(format: &str, line: &str) -> String {
    let readable = |name: &str| field(format, line, name).and_then(|v| parse_colour(v.trim()));
    if readable("PrimaryColour").is_none() || readable("OutlineColour").is_none() {
        return line.to_owned();
    }
    let light = style_is_light(format, line);
    let Some((head, values)) = line.split_once(':') else {
        return line.to_owned();
    };
    let names: Vec<&str> = match format.split_once(':') {
        Some((_, cols)) => cols.split(',').map(str::trim).collect(),
        None => return line.to_owned(),
    };
    let out: Vec<String> = values
        .split(',')
        .enumerate()
        .map(|(i, v)| {
            let kind = match names.get(i).map(|n| n.to_ascii_lowercase()) {
                Some(n) if n == "primarycolour" => Some(Part::Fill),
                Some(n) if n == "outlinecolour" || n == "backcolour" => Some(Part::Edge),
                _ => None,
            };
            match (kind, parse_colour(v.trim())) {
                (Some(part), Some(c)) => {
                    let lead = &v[..v.len() - v.trim_start().len()];
                    format!("{lead}{}", style_colour(nudge(c, part, light)))
                }
                _ => v.to_owned(),
            }
        })
        .collect();
    format!("{head}:{}", out.join(","))
}

/// An event's text with its inline `\c`/`\1c` (fill) and `\3c`/`\4c`
/// (outline, shadow) colours nudged apart; unchanged when it draws a shape.
pub(crate) fn event_text(text: &str, light: bool) -> String {
    if draws(text) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for piece in tags::pieces(text) {
        match piece {
            Piece::Text(t) => out.push_str(t),
            Piece::Block(b) => {
                out.push('{');
                out.push_str(&nudge_block(b, light));
                out.push('}');
            }
        }
    }
    out
}

/// Whether an event's text draws a shape (`\p` above 0 anywhere).
pub(crate) fn draws(text: &str) -> bool {
    tags::scan_text(text).iter().any(|t| {
        t.name == "p"
            && t.arg
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .is_ok_and(|n| n > 0)
    })
}

#[derive(Clone, Copy)]
enum Part {
    Fill,
    Edge,
}

/// Rewrites `\Nc&H..&` colour values inside one override block.
fn nudge_block(block: &str, light: bool) -> String {
    let mut out = String::with_capacity(block.len());
    let mut rest = block;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let (part, name_len) = if tail.starts_with("1c") {
            (Some(Part::Fill), 2)
        } else if tail.starts_with("3c") || tail.starts_with("4c") {
            (Some(Part::Edge), 2)
        } else if tail.starts_with('c') && !tail.starts_with("clip") {
            (Some(Part::Fill), 1)
        } else {
            (None, 0)
        };
        let value = &tail[name_len..];
        let end = value.find('\\').unwrap_or(value.len());
        let colour = part.and_then(|_| {
            let v = value[..end].trim_end_matches(')');
            parse_colour(v).map(|c| (c, v.len()))
        });
        match (part, colour) {
            (Some(p), Some((c, used))) => {
                out.push('\\');
                out.push_str(&tail[..name_len]);
                out.push_str(&inline_colour(nudge(c, p, light)));
                rest = &value[used..];
            }
            _ => {
                out.push('\\');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `(alpha, b, g, r)`.
type Colour = (u8, u8, u8, u8);

/// `&HAABBGGRR`, `&HBBGGRR&`, with or without the `&`s.
fn parse_colour(v: &str) -> Option<Colour> {
    let hex = v
        .trim_start_matches('&')
        .strip_prefix(['H', 'h'])?
        .trim_end_matches('&');
    if hex.is_empty() || hex.len() > 8 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    let [a, b, g, r] = n.to_be_bytes();
    Some((a, b, g, r))
}

fn style_colour((a, b, g, r): Colour) -> String {
    format!("&H{a:02X}{b:02X}{g:02X}{r:02X}")
}

fn inline_colour((_, b, g, r): Colour) -> String {
    format!("&H{b:02X}{g:02X}{r:02X}&")
}

fn luma((_, b, g, r): Colour) -> f32 {
    0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
}

fn nudge((a, b, g, r): Colour, part: Part, light: bool) -> Colour {
    let toward_white = matches!((part, light), (Part::Fill, true) | (Part::Edge, false));
    let target = if toward_white { 255.0 } else { 0.0 };
    let share = match part {
        Part::Fill => FILL_SHIFT,
        Part::Edge => EDGE_SHIFT,
    };
    let mix = |c: u8| (f32::from(c) + (target - f32::from(c)) * share).round() as u8;
    (a, mix(b), mix(g), mix(r))
}

fn field<'a>(format: &str, line: &'a str, name: &str) -> Option<&'a str> {
    let at = format
        .split_once(':')?
        .1
        .split(',')
        .position(|c| c.trim().eq_ignore_ascii_case(name))?;
    line.split_once(':')?.1.split(',').nth(at)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const FORMAT: &str = "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold";

    #[test]
    fn a_light_sign_gets_a_lighter_fill_and_darker_edge() {
        let style = "Style: Menu,Times New Roman,60,&H00F4A98A,&H0000FFFF,&H2B4B321D,&H7F4B321D,-1";
        assert!(style_is_light(FORMAT, style));
        let out = style_line(FORMAT, style);
        assert_eq!(
            out,
            "Style: Menu,Times New Roman,60,&H00F7BFA7,&H0000FFFF,&H2B1E140C,&H7F1E140C,-1"
        );
        // Each fill channel rises, each edge channel falls; alpha is kept.
        let fill = parse_colour("&H00F7BFA7").unwrap();
        assert!(luma(fill) > luma(parse_colour("&H00F4A98A").unwrap()));
    }

    #[test]
    fn a_dark_sign_goes_the_other_way() {
        let style = "Style: Ink,Arial,40,&H00202020,&H0000FFFF,&H00E0E0E0,&H00000000,0";
        assert!(!style_is_light(FORMAT, style));
        let out = style_line(FORMAT, style);
        assert!(out.contains(",&H00181818,"), "{out}");
        assert!(out.contains(",&H00F3F3F3,"), "{out}");
    }

    #[test]
    fn inline_colours_are_nudged_and_drawings_kept() {
        assert_eq!(
            event_text(
                r"{\1a&H17&\c&HB98B75&}A {\3c&H000000&\clip(0,0,5,5)}B",
                true
            ),
            r"{\1a&H17&\c&HCBA898&}A {\3c&H000000&\clip(0,0,5,5)}B"
        );
        let shape = r"{\bord0\p1)}m 0 0 l 510 0 510 420 0 420";
        assert_eq!(event_text(shape, true), shape);
    }

    #[test]
    fn unreadable_colours_are_left_as_they_are() {
        let style = "Style: Odd,Arial,40,red,&H0000FFFF,&H00E0E0E0,&H00000000,0";
        assert_eq!(style_line(FORMAT, style), style);
        assert_eq!(event_text(r"{\c&Hzz&}x", true), r"{\c&Hzz&}x");
    }
}
