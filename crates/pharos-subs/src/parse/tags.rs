//! ASS dialogue text: `{\tag}` override blocks, `\N \n \h`, plain tier only.

use std::sync::Arc;

use crate::model::{Overrides, ParseStats, Run};

pub(crate) struct ParsedText {
    pub text: String,
    pub runs: Vec<Run>,
    pub alignment: Option<u8>,
}

/// Parses `raw` (the Text field of a Dialogue line) into visible text and runs.
pub(crate) fn parse_text(raw: &str, wrap_style: u8, stats: &mut ParseStats) -> ParsedText {
    let mut text = String::with_capacity(raw.len());
    let mut runs: Vec<Run> = Vec::new();
    let mut cur = Overrides::default();
    let mut alignment = None;
    let mut run_open = 0usize; // text length where the current run started

    let flush = |text: &String, runs: &mut Vec<Run>, cur: &Overrides, run_open: &mut usize| {
        if text.len() > *run_open {
            match runs.last_mut() {
                Some(last) if last.ov == *cur => last.end = text.len(),
                _ => runs.push(Run {
                    end: text.len(),
                    ov: cur.clone(),
                }),
            }
            *run_open = text.len();
        }
    };

    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes.get(i) {
            Some(b'{') => {
                if let Some(rel) = raw.get(i..).and_then(|s| s.find('}')) {
                    flush(&text, &mut runs, &cur, &mut run_open);
                    let block = raw.get(i + 1..i + rel).unwrap_or("");
                    parse_block(block, &mut cur, &mut alignment, stats);
                    i += rel + 1;
                } else {
                    text.push('{');
                    i += 1;
                }
            }
            Some(b'\\') => match bytes.get(i + 1) {
                Some(b'N') => {
                    text.push('\n');
                    i += 2;
                }
                Some(b'n') => {
                    text.push(if wrap_style == 2 { '\n' } else { ' ' });
                    i += 2;
                }
                Some(b'h') => {
                    text.push('\u{a0}');
                    i += 2;
                }
                _ => {
                    text.push('\\');
                    i += 1;
                }
            },
            _ => {
                // Copy the next char whole (UTF-8 safe) up to the next special byte.
                let rest = raw.get(i..).unwrap_or("");
                let n = rest
                    .find(['{', '\\'])
                    .map_or(rest.len(), |p| if p == 0 { 1 } else { p });
                text.push_str(rest.get(..n).unwrap_or(""));
                i += n;
            }
        }
    }
    flush(&text, &mut runs, &cur, &mut run_open);
    ParsedText {
        text,
        runs,
        alignment,
    }
}

fn parse_block(
    block: &str,
    cur: &mut Overrides,
    alignment: &mut Option<u8>,
    stats: &mut ParseStats,
) {
    let mut depth = 0i32;
    let mut start = None;
    let b = block.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'\\' if depth <= 0 => {
                if let Some(s) = start {
                    apply_tag(block.get(s..i).unwrap_or(""), cur, alignment, stats);
                }
                start = Some(i + 1);
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        apply_tag(block.get(s..).unwrap_or(""), cur, alignment, stats);
    }
}

fn num(s: &str) -> Option<f32> {
    let s = s.trim();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && (c == '-' || c == '+'))))
        .map_or(s.len(), |(i, _)| i);
    s.get(..end)?.parse().ok()
}

/// `&HBBGGRR&` -> RGB.
pub(crate) fn parse_bgr(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().trim_matches('&');
    let s = s.strip_prefix(['H', 'h']).unwrap_or(s);
    let v = u32::from_str_radix(s.trim_end_matches('&'), 16).ok()?;
    Some([
        (v & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        ((v >> 16) & 0xff) as u8,
    ])
}

fn parse_alpha(s: &str) -> Option<u8> {
    let s = s.trim().trim_matches('&');
    let s = s.strip_prefix(['H', 'h']).unwrap_or(s);
    let v = u32::from_str_radix(s.trim_end_matches('&'), 16).ok()?;
    Some(255 - (v & 0xff) as u8)
}

fn flag(v: &str) -> Option<bool> {
    // `\b700` style weights count as bold from 600.
    num(v).map(|n| n >= 1.0 && (n <= 1.0 || n >= 600.0))
}

fn apply_tag(tag: &str, cur: &mut Overrides, alignment: &mut Option<u8>, stats: &mut ParseStats) {
    let tag = tag.trim_end();
    if tag.is_empty() {
        return;
    }
    let after = |n: usize| tag.get(n..).unwrap_or("");
    let starts = |p: &str| tag.starts_with(p);
    let next_is_value = |n: usize| {
        tag.as_bytes()
            .get(n)
            .map_or(true, |c| c.is_ascii_digit() || *c == b'-' || *c == b'+')
    };

    if starts("alpha") {
        let v = after(5);
        let a = if v.is_empty() { None } else { parse_alpha(v) };
        cur.alpha = [a; 4];
    } else if starts("an") && next_is_value(2) {
        if let Some(n) = num(after(2)) {
            if (1.0..=9.0).contains(&n) {
                *alignment = Some(n as u8);
            }
        }
    } else if let Some(slot) = slot_of(tag, b'c') {
        let v = after(2);
        cur.rgb[slot] = if v.is_empty() { None } else { parse_bgr(v) };
    } else if let Some(slot) = slot_of(tag, b'a') {
        let v = after(2);
        cur.alpha[slot] = if v.is_empty() { None } else { parse_alpha(v) };
    } else if starts("fn") {
        let v = after(2).trim();
        cur.font = if v.is_empty() {
            None
        } else {
            Some(Arc::from(v))
        };
    } else if starts("fs") && next_is_value(2) {
        cur.size = num(after(2)).filter(|s| *s > 0.0);
    } else if starts("bord") {
        cur.bord = num(after(4)).map(|v| v.max(0.0));
    } else if starts("shad") {
        cur.shad = num(after(4)).map(|v| v.max(0.0));
    } else if starts("b") && next_is_value(1) {
        cur.bold = flag(after(1));
    } else if starts("i") && next_is_value(1) {
        cur.italic = flag(after(1));
    } else if starts("u") && next_is_value(1) {
        cur.underline = flag(after(1));
    } else if starts("s") && next_is_value(1) {
        cur.strike = flag(after(1));
    } else if starts("c") && tag.as_bytes().get(1).map_or(true, |c| *c == b'&') {
        let v = after(1);
        cur.rgb[0] = if v.is_empty() { None } else { parse_bgr(v) };
    } else if starts("r") && !starts("rnd") {
        *cur = Overrides::default();
    } else {
        let name: String = tag
            .chars()
            .take_while(|c| c.is_ascii_alphabetic() || c.is_ascii_digit())
            .take(8)
            .collect();
        // Leading digit tags (`1vc`) keep their digit; strip trailing numerals of
        // alphabetic names so `fscx120` and `fscx80` count as one tag.
        let name = if name.starts_with(|c: char| c.is_ascii_digit()) {
            name
        } else {
            name.trim_end_matches(|c: char| c.is_ascii_digit())
                .to_owned()
        };
        stats.unsupported(&name);
    }
}

/// `1c`..`4c` / `1a`..`4a` -> slot 0..3.
fn slot_of(tag: &str, kind: u8) -> Option<usize> {
    let b = tag.as_bytes();
    let d = *b.first()?;
    if (b'1'..=b'4').contains(&d) && b.get(1) == Some(&kind) {
        Some(usize::from(d - b'1'))
    } else {
        None
    }
}
