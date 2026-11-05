//! ASS v4+ and SSA v4 scripts.

use super::tags::{parse_bgr, parse_text};
use crate::model::{Event, Format, Rgba, Script, Style};

#[derive(PartialEq, Clone, Copy)]
enum Section {
    None,
    Info,
    Styles,
    Events,
    Other,
}

pub(crate) fn parse(src: &str) -> Script {
    let mut script = Script::empty(Format::Ass);
    script.styles.clear();
    let mut section = Section::None;
    let mut style_fmt: Vec<String> = Vec::new();
    let mut event_fmt: Vec<String> = Vec::new();
    let mut ssa = false;

    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('!') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line.trim_matches(['[', ']']).to_ascii_lowercase();
            section = match name.as_str() {
                "script info" => Section::Info,
                "v4+ styles" | "v4 styles" | "v4++ styles" => {
                    if name == "v4 styles" {
                        ssa = true;
                    }
                    Section::Styles
                }
                "events" => Section::Events,
                _ => Section::Other,
            };
            continue;
        }
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim_start();
        match section {
            Section::Info => info_line(&mut script, key, val, &mut ssa),
            Section::Styles => match key.to_ascii_lowercase().as_str() {
                "format" => style_fmt = split_format(val),
                "style" => match parse_style(val, &style_fmt, ssa) {
                    Some(s) => script.styles.push(s),
                    None => script.stats.bad_lines += 1,
                },
                _ => {}
            },
            Section::Events => match key.to_ascii_lowercase().as_str() {
                "format" => event_fmt = split_format(val),
                "dialogue" => match parse_event(&mut script, val, &event_fmt) {
                    Some(e) => script.events.push(e),
                    None => script.stats.bad_lines += 1,
                },
                _ => {}
            },
            _ => {}
        }
    }
    if ssa {
        script.format = Format::Ssa;
    }
    if script.styles.is_empty() {
        script.styles.push(Style::default());
    }
    script.finish();
    script
}

fn info_line(script: &mut Script, key: &str, val: &str, ssa: &mut bool) {
    match key.to_ascii_lowercase().as_str() {
        "playresx" => script.play_res_x = val.trim().parse().unwrap_or(script.play_res_x),
        "playresy" => script.play_res_y = val.trim().parse().unwrap_or(script.play_res_y),
        "wrapstyle" => script.wrap_style = val.trim().parse::<u8>().unwrap_or(0).min(3),
        "scaledborderandshadow" => {
            script.scaled_border_and_shadow = val.trim().eq_ignore_ascii_case("yes");
        }
        "scripttype" => *ssa = val.trim().eq_ignore_ascii_case("v4.00"),
        _ => {}
    }
}

fn split_format(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .collect()
}

/// `&HAABBGGRR` hex or signed decimal -> opaque-255 RGBA.
fn parse_colour(s: &str) -> Option<Rgba> {
    let s = s.trim();
    let (rgb, alpha_ass) = if s.starts_with('&') {
        let t = s.trim_matches('&');
        let t = t.strip_prefix(['H', 'h']).unwrap_or(t);
        let v = u32::from_str_radix(t, 16).ok()?;
        (
            parse_bgr(&format!("{:06x}", v & 0xff_ffff))?,
            (v >> 24) as u8,
        )
    } else {
        let v = s.parse::<i64>().ok()? as u32;
        (
            parse_bgr(&format!("{:06x}", v & 0xff_ffff))?,
            (v >> 24) as u8,
        )
    };
    Some([rgb[0], rgb[1], rgb[2], 255 - alpha_ass])
}

fn parse_style(val: &str, fmt: &[String], ssa: bool) -> Option<Style> {
    let fields: Vec<&str> = val.split(',').map(str::trim).collect();
    let mut s = Style::default();
    for (name, v) in fmt.iter().zip(fields.iter()) {
        let flag = |v: &str| v.parse::<i32>().is_ok_and(|n| n != 0);
        match name.as_str() {
            "name" => s.name = (*v).to_owned(),
            "fontname" => s.font = (*v).to_owned(),
            "fontsize" => s.size = v.parse().ok()?,
            "primarycolour" => s.colours[0] = parse_colour(v)?,
            "secondarycolour" => s.colours[1] = parse_colour(v)?,
            "outlinecolour" | "tertiarycolour" => s.colours[2] = parse_colour(v)?,
            "backcolour" => s.colours[3] = parse_colour(v)?,
            "bold" => s.bold = flag(v),
            "italic" => s.italic = flag(v),
            "underline" => s.underline = flag(v),
            "strikeout" => s.strike = flag(v),
            "outline" => s.outline = v.parse().ok()?,
            "shadow" => s.shadow = v.parse().ok()?,
            "alignment" => {
                let a: u8 = v.parse().ok()?;
                s.alignment = if ssa { ssa_alignment(a) } else { a.clamp(1, 9) };
            }
            "marginl" => s.margin_l = v.parse().ok()?,
            "marginr" => s.margin_r = v.parse().ok()?,
            "marginv" => s.margin_v = v.parse().ok()?,
            _ => {}
        }
    }
    Some(s)
}

/// SSA v4 legacy alignment (1-3 bottom, 5-7 top, 9-11 middle) to numpad.
fn ssa_alignment(a: u8) -> u8 {
    let horiz = match a & 3 {
        1 => 0,
        2 => 1,
        _ => 2,
    };
    let row = match a {
        5..=7 => 6,
        9..=11 => 3,
        _ => 0,
    };
    row + horiz + 1
}

fn parse_time(s: &str) -> Option<i64> {
    let mut parts = s.trim().split(':');
    let h: i64 = parts.next()?.trim().parse().ok()?;
    let m: i64 = parts.next()?.trim().parse().ok()?;
    let sec = parts.next()?.trim();
    let (whole, frac) = sec.split_once('.').unwrap_or((sec, "0"));
    let whole: i64 = whole.parse().ok()?;
    // Centiseconds normally; scale any digit count to ms.
    let mut ms: i64 = 0;
    let mut scale = 100;
    for c in frac.chars().take(3) {
        ms += i64::from(c.to_digit(10)?) * scale;
        scale /= 10;
    }
    Some(((h * 60 + m) * 60 + whole) * 1000 + ms)
}

fn parse_event(script: &mut Script, val: &str, fmt: &[String]) -> Option<Event> {
    let n = if fmt.is_empty() { 10 } else { fmt.len() };
    let default_fmt: Vec<String>;
    let fmt = if fmt.is_empty() {
        default_fmt = [
            "layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect",
            "text",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        &default_fmt
    } else {
        fmt
    };
    let fields: Vec<&str> = val.splitn(n, ',').collect();
    if fields.len() < n {
        return None;
    }
    let mut ev = Event {
        start_ms: 0,
        end_ms: 0,
        layer: 0,
        style: 0,
        margin_l: 0,
        margin_r: 0,
        margin_v: 0,
        alignment: None,
        text: String::new(),
        runs: Vec::new(),
    };
    let mut raw_text = "";
    for (name, v) in fmt.iter().zip(fields.iter()) {
        match name.as_str() {
            "layer" | "marked" => {
                ev.layer = v.trim().trim_start_matches("Marked=").parse().unwrap_or(0)
            }
            "start" => ev.start_ms = parse_time(v)?,
            "end" => ev.end_ms = parse_time(v)?,
            "style" => {
                let want = v.trim().trim_start_matches('*');
                ev.style = script
                    .styles
                    .iter()
                    .position(|s| s.name.eq_ignore_ascii_case(want))
                    .unwrap_or(0);
            }
            "marginl" => ev.margin_l = v.trim().parse().unwrap_or(0),
            "marginr" => ev.margin_r = v.trim().parse().unwrap_or(0),
            "marginv" => ev.margin_v = v.trim().parse().unwrap_or(0),
            "text" => raw_text = v,
            _ => {}
        }
    }
    if ev.end_ms <= ev.start_ms {
        return None;
    }
    let parsed = parse_text(raw_text, script.wrap_style, &mut script.stats);
    ev.text = parsed.text;
    ev.runs = parsed.runs;
    ev.alignment = parsed.alignment;
    Some(ev)
}
