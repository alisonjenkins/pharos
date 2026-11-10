//! Reads SRT, WebVTT and ASS into [`SourceLine`]s (spec 028 T028-02).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::tags::{self, Piece};

/// Which format a line came from; decides how `raw` is read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFormat {
    Srt,
    Vtt,
    Ass,
}

/// One karaoke syllable: how long it is sung and the text it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Syllable {
    pub dur_ms: u32,
    pub text: String,
}

/// One cue or dialogue event of the source track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLine {
    pub start_ms: i64,
    pub end_ms: i64,
    /// Visible text with every tag stripped; hard breaks are `'\n'`.
    pub text: String,
    /// The original text: the ASS Text field, or the SRT/VTT cue text.
    pub raw: String,
    pub format: SourceFormat,
    /// ASS style name, `"Default"` for SRT and VTT.
    pub style: String,
    /// ASS layer, 0 for SRT and VTT.
    pub layer: i32,
    /// All visible text is italic (`<i>`, `{\i1}`).
    pub italic: bool,
    /// An ASS event that carries its own typesetting: `\pos`, `\move`, `\an`
    /// (or legacy `\a`) other than 2, `\clip`, `\iclip`, `\p` or `\org`.
    pub typeset: bool,
    /// Syllable timings from `\k`, `\kf`, `\K` or `\ko`.
    pub karaoke: Option<Vec<Syllable>>,
    /// The source's definition of `style`, verbatim, for ASS lines whose
    /// style the script defines. Typeset events keep it (FR-005).
    pub style_def: Option<Arc<SourceStyle>>,
}

/// A `Style:` line of the source as written, with the `Format:` line that
/// gives its columns.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceStyle {
    pub format: String,
    pub line: String,
}

impl SourceStyle {
    /// The style's `Alignment` column; `None` when the line has none or it
    /// is not a number.
    pub fn alignment(&self) -> Option<u8> {
        let columns = self.format.split_once(':')?.1.split(',').map(str::trim);
        let at = columns
            .into_iter()
            .position(|c| c.eq_ignore_ascii_case("alignment"))?;
        self.line
            .split_once(':')?
            .1
            .split(',')
            .nth(at)
            .and_then(|v| lead_int(v.trim()))
    }
}

/// The leading decimal digits of `s` as libass reads a numeric tag or field
/// (`1)` is 1); `None` without any.
fn lead_int<T: std::str::FromStr>(s: &str) -> Option<T> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// What [`read`] found: the lines, and how many cues or events it had to skip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub lines: Vec<SourceLine>,
    /// Cues or dialogue events that could not be read (bad timestamp, too
    /// few fields) and were left out. A caller can log this.
    pub skipped: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SourceError {
    #[error("line {line}: cannot read the timestamp {text:?}")]
    BadTimestamp { line: usize, text: String },
    #[error("line {line}: dialogue event has too few fields")]
    BadEvent { line: usize },
    #[error("not a subtitle script: no cue timing line and no [Events] section")]
    Unrecognised,
}

/// A run of visible text that shares one italic state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub text: String,
    pub italic: bool,
}

/// Reads `script`, sniffing the format: ASS when it has `[Script Info]` or
/// `[Events]`, WebVTT when it starts with `WEBVTT`, SRT otherwise.
///
/// A cue or event that cannot be read is skipped and counted in
/// [`Source::skipped`]. Only text with no cue at all (not even a bad one) and
/// no `[Events]` section is an error.
pub fn read(script: &str) -> Result<Source, SourceError> {
    let script = script.strip_prefix('\u{feff}').unwrap_or(script);
    if script.trim_start().starts_with("WEBVTT") {
        Ok(read_cues(script, SourceFormat::Vtt))
    } else if script.contains("[Script Info]") || script.contains("[Events]") {
        Ok(read_ass(script))
    } else if script.trim().is_empty() {
        Ok(Source {
            lines: Vec::new(),
            skipped: 0,
        })
    } else if script.contains("-->") {
        Ok(read_cues(script, SourceFormat::Srt))
    } else {
        Err(SourceError::Unrecognised)
    }
}

/// `H:MM:SS.cc`, `HH:MM:SS,mmm` or `MM:SS.mmm` to milliseconds. Digits only,
/// hours up to 99, minutes and seconds up to 99 (ASS writers are loose), at
/// most three fraction digits.
pub(crate) fn parse_time(s: &str) -> Option<i64> {
    let digits = |t: &str, max: i64| -> Option<i64> {
        let t = t.trim();
        if t.is_empty() || t.len() > 9 || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        t.parse::<i64>().ok().filter(|&n| n <= max)
    };
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [m, sec] => ("0", *m, *sec),
        [h, m, sec] => (*h, *m, *sec),
        _ => return None,
    };
    let (whole, frac) = match sec.split_once(['.', ',']) {
        Some((w, f)) if !f.is_empty() && f.len() <= 3 => (w, f),
        Some(_) => return None,
        None => (sec, ""),
    };
    let mut ms = 0i64;
    let mut scale = 100;
    for c in frac.chars() {
        ms += i64::from(c.to_digit(10)?) * scale;
        scale /= 10;
    }
    let total = digits(h, 99)?
        .checked_mul(60)?
        .checked_add(digits(m, 99)?)?
        .checked_mul(60)?
        .checked_add(digits(whole, 99)?)?
        .checked_mul(1000)?
        .checked_add(ms)?;
    Some(total)
}

// ---------------------------------------------------------------- SRT / VTT

fn read_cues(script: &str, format: SourceFormat) -> Source {
    let lines: Vec<&str> = script.lines().collect();
    let mut out = Vec::new();
    let mut skipped = 0;
    let mut i = 0;
    while i < lines.len() {
        let Some((a, b)) = lines[i].split_once("-->") else {
            i += 1;
            continue;
        };
        let end_tok = b.split_whitespace().next().unwrap_or("");
        let times = parse_time(a).zip(parse_time(end_tok));
        // The body ends at a blank line, at the next timing line, or (SRT) at
        // the number line that introduces the next cue.
        let mut body = Vec::new();
        i += 1;
        while i < lines.len() {
            let l = lines[i];
            let next_is_timing = lines.get(i + 1).is_some_and(|n| n.contains("-->"));
            if l.trim().is_empty()
                || l.contains("-->")
                || (format == SourceFormat::Srt
                    && next_is_timing
                    && l.trim().bytes().all(|b| b.is_ascii_digit()))
            {
                break;
            }
            body.push(l.trim_end());
            i += 1;
        }
        match times {
            Some((start, end)) => out.push(build_line(
                start,
                end,
                body.join("\n"),
                format,
                "Default".to_owned(),
                0,
                None,
            )),
            None => skipped += 1,
        }
    }
    Source {
        lines: out,
        skipped,
    }
}

// ---------------------------------------------------------------------- ASS

/// A `Dialogue:` event as written, before its text is interpreted.
pub(crate) struct AssEvent<'a> {
    pub layer: i32,
    pub start_ms: i64,
    pub end_ms: i64,
    pub style: &'a str,
    pub name: &'a str,
    pub text: &'a str,
}

const DEFAULT_EVENT_FORMAT: [&str; 10] = [
    "layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text",
];

struct AssScan<'a> {
    events: Vec<AssEvent<'a>>,
    /// Dialogue lines that could not be read.
    skipped: usize,
    first_error: Option<SourceError>,
    styles: BTreeMap<String, Arc<SourceStyle>>,
}

/// Walks the script once: `Dialogue:` lines of `[Events]` (honouring its
/// `Format:` column order; the Text column is last and may contain commas)
/// and the `Style:` lines of the styles section, kept verbatim.
fn scan_ass(script: &str) -> AssScan<'_> {
    #[derive(PartialEq)]
    enum Sec {
        Other,
        Styles,
        Events,
    }
    let mut sec = Sec::Other;
    let mut fmt: Vec<String> = DEFAULT_EVENT_FORMAT
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let mut style_fmt_line = String::new();
    let mut style_name_col = 0usize;
    let mut scan = AssScan {
        events: Vec::new(),
        skipped: 0,
        first_error: None,
        styles: BTreeMap::new(),
    };
    for (idx, line) in script.lines().enumerate() {
        let line_no = idx + 1;
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            let n = t.to_ascii_lowercase();
            sec = if n.contains("styles") {
                Sec::Styles
            } else if n == "[events]" {
                Sec::Events
            } else {
                Sec::Other
            };
            continue;
        }
        let Some((key, val)) = t.split_once(':') else {
            continue;
        };
        let val = val.trim_start();
        let key = key.trim().to_ascii_lowercase();
        match (&sec, key.as_str()) {
            (Sec::Styles, "format") => {
                style_fmt_line = t.to_owned();
                style_name_col = val
                    .split(',')
                    .position(|c| c.trim().eq_ignore_ascii_case("name"))
                    .unwrap_or(0);
            }
            (Sec::Styles, "style") => {
                if let Some(name) = val.split(',').nth(style_name_col) {
                    scan.styles.insert(
                        name.trim().to_owned(),
                        Arc::new(SourceStyle {
                            format: style_fmt_line.clone(),
                            line: t.to_owned(),
                        }),
                    );
                }
            }
            (Sec::Events, "format") => {
                fmt = val
                    .split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .collect();
            }
            (Sec::Events, "dialogue") => match parse_dialogue(val, &fmt, line_no) {
                Ok(ev) => scan.events.push(ev),
                Err(e) => {
                    scan.skipped += 1;
                    scan.first_error.get_or_insert(e);
                }
            },
            _ => {}
        }
    }
    scan
}

fn parse_dialogue<'a>(
    val: &'a str,
    fmt: &[String],
    line_no: usize,
) -> Result<AssEvent<'a>, SourceError> {
    let fields: Vec<&str> = val.splitn(fmt.len(), ',').collect();
    if fields.len() < fmt.len() {
        return Err(SourceError::BadEvent { line: line_no });
    }
    let mut ev = AssEvent {
        layer: 0,
        start_ms: 0,
        end_ms: 0,
        style: "Default",
        name: "",
        text: "",
    };
    for (name, v) in fmt.iter().zip(fields) {
        let time = |v: &str| {
            parse_time(v).ok_or_else(|| SourceError::BadTimestamp {
                line: line_no,
                text: v.trim().to_owned(),
            })
        };
        match name.as_str() {
            "layer" | "marked" => {
                ev.layer = v.trim().trim_start_matches("Marked=").parse().unwrap_or(0);
            }
            "start" => ev.start_ms = time(v)?,
            "end" => ev.end_ms = time(v)?,
            "style" => ev.style = v.trim().trim_start_matches('*'),
            "name" => ev.name = v.trim(),
            "text" => ev.text = v,
            _ => {}
        }
    }
    Ok(ev)
}

/// The `Dialogue:` lines of the file, or the first one that cannot be read.
/// Used on our own output, where any unreadable line is a bug.
pub(crate) fn ass_events(script: &str) -> Result<Vec<AssEvent<'_>>, SourceError> {
    let scan = scan_ass(script);
    match scan.first_error {
        Some(e) => Err(e),
        None => Ok(scan.events),
    }
}

fn read_ass(script: &str) -> Source {
    let scan = scan_ass(script);
    let lines = scan
        .events
        .into_iter()
        .map(|e| {
            build_line(
                e.start_ms,
                e.end_ms,
                e.text.to_owned(),
                SourceFormat::Ass,
                e.style.to_owned(),
                e.layer,
                scan.styles.get(e.style).cloned(),
            )
        })
        .collect();
    Source {
        lines,
        skipped: scan.skipped,
    }
}

// ------------------------------------------------------------- interpretation

struct Parsed {
    runs: Vec<Run>,
    typeset: bool,
    /// Whether the last alignment override puts the line at bottom centre;
    /// `None` without one, when the style's alignment decides.
    bottom_centre: Option<bool>,
    karaoke: Option<Vec<Syllable>>,
}

fn build_line(
    start_ms: i64,
    end_ms: i64,
    raw: String,
    format: SourceFormat,
    style: String,
    layer: i32,
    style_def: Option<Arc<SourceStyle>>,
) -> SourceLine {
    let p = parse_text(&raw, format);
    // A style placed anywhere but bottom centre is a sign's (B1208), unless
    // the line moves itself back there.
    let style_placed = p.bottom_centre.is_none()
        && style_def
            .as_deref()
            .and_then(SourceStyle::alignment)
            .is_some_and(|a| a != 2);
    let text = join(&p.runs);
    let visible: Vec<&Run> = p
        .runs
        .iter()
        .filter(|r| !r.text.trim().is_empty())
        .collect();
    let italic = !visible.is_empty() && visible.iter().all(|r| r.italic);
    SourceLine {
        start_ms,
        end_ms,
        text,
        raw,
        format,
        style,
        layer,
        italic,
        typeset: p.typeset || style_placed,
        karaoke: p.karaoke,
        style_def,
    }
}

fn join(runs: &[Run]) -> String {
    let all: String = runs.iter().map(|r| r.text.as_str()).collect();
    all.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// The visible runs of a line, outer whitespace trimmed, for the compiler.
pub(crate) fn runs(line: &SourceLine) -> Vec<Run> {
    let mut runs = parse_text(&line.raw, line.format).runs;
    if let Some(first) = runs.first_mut() {
        first.text = first.text.trim_start().to_owned();
    }
    if let Some(last) = runs.last_mut() {
        last.text = last.text.trim_end().to_owned();
    }
    runs.retain(|r| !r.text.is_empty());
    runs
}

#[derive(Default)]
struct Acc {
    runs: Vec<Run>,
    italic: bool,
}

impl Acc {
    fn push(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        match self.runs.last_mut() {
            Some(r) if r.italic == self.italic => r.text.push_str(s),
            _ => self.runs.push(Run {
                text: s.to_owned(),
                italic: self.italic,
            }),
        }
    }
}

fn parse_text(raw: &str, format: SourceFormat) -> Parsed {
    match format {
        SourceFormat::Ass => parse_ass_text(raw),
        SourceFormat::Srt | SourceFormat::Vtt => parse_cue_text(raw),
    }
}

fn parse_ass_text(raw: &str) -> Parsed {
    let mut acc = Acc::default();
    let mut typeset = false;
    let mut bottom_centre = None;
    let mut drawing = false;
    let mut syllables: Vec<Syllable> = Vec::new();
    let mut pending: Option<u32> = None; // duration of the open syllable
    let mut buf = String::new();
    let mut saw_k = false;

    for piece in tags::pieces(raw) {
        match piece {
            Piece::Text(t) => {
                if drawing {
                    continue;
                }
                let text = unescape_ass(t);
                acc.push(&text);
                buf.push_str(&text);
            }
            Piece::Block(b) => {
                for tag in tags::scan(b) {
                    if tag.nested {
                        continue;
                    }
                    match tag.name.as_str() {
                        "i" => acc.italic = tag.arg == "1",
                        "r" => acc.italic = false,
                        "pos" | "move" | "org" | "clip" | "iclip" => typeset = true,
                        // Legacy SSA `\a` too: 2 is bottom centre in both.
                        "an" | "a" => {
                            if let Some(n) = lead_int::<u8>(&tag.arg) {
                                bottom_centre = Some(n == 2);
                            }
                        }
                        "p" => {
                            drawing = lead_int::<u32>(&tag.arg).is_some_and(|n| n > 0);
                            typeset |= drawing;
                        }
                        "k" | "kf" | "K" | "ko" => {
                            if saw_k || !buf.is_empty() {
                                syllables.push(Syllable {
                                    dur_ms: pending.unwrap_or(0),
                                    text: std::mem::take(&mut buf),
                                });
                            }
                            saw_k = true;
                            pending = Some(tag.arg.parse::<u32>().unwrap_or(0).saturating_mul(10));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    if saw_k {
        syllables.push(Syllable {
            dur_ms: pending.unwrap_or(0),
            text: buf,
        });
    }
    Parsed {
        runs: acc.runs,
        typeset: typeset || bottom_centre == Some(false),
        bottom_centre,
        karaoke: saw_k.then_some(syllables),
    }
}

fn unescape_ass(t: &str) -> String {
    t.replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
}

fn parse_cue_text(raw: &str) -> Parsed {
    let mut acc = Acc::default();
    let mut rest = raw;
    let mut plain = String::new();
    loop {
        let Some(pos) = rest.find(['<', '{', '&']) else {
            plain.push_str(rest);
            break;
        };
        plain.push_str(&rest[..pos]);
        rest = &rest[pos..];
        match rest.as_bytes()[0] {
            b'<' => match rest.find('>').filter(|&e| is_html_tag(&rest[1..e])) {
                Some(e) => {
                    let name = rest[1..e]
                        .trim_start_matches('/')
                        .split(|c: char| c.is_whitespace() || c == '.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if name == "i" || name == "em" {
                        acc.push(&std::mem::take(&mut plain));
                        acc.italic = !rest[1..].starts_with('/');
                    }
                    rest = &rest[e + 1..];
                }
                None => {
                    plain.push('<');
                    rest = &rest[1..];
                }
            },
            b'{' => match rest.find('}') {
                Some(e) => {
                    acc.push(&std::mem::take(&mut plain));
                    for tag in tags::scan(&rest[1..e]) {
                        if tag.name == "i" {
                            acc.italic = tag.arg == "1";
                        }
                    }
                    rest = &rest[e + 1..];
                }
                None => {
                    plain.push('{');
                    rest = &rest[1..];
                }
            },
            _ => {
                let (ch, len) = entity(rest);
                plain.push_str(&ch);
                rest = &rest[len..];
            }
        }
    }
    acc.push(&plain);
    Parsed {
        runs: acc.runs,
        typeset: false,
        bottom_centre: None,
        karaoke: None,
    }
}

/// `<i>`, `</i>`, `<c.yellow>`, `<v Name>`, `<00:01.000>`: not `<3` or `a < b`.
fn is_html_tag(inner: &str) -> bool {
    let t = inner.trim_start_matches('/');
    t.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Decodes the entity at the start of `rest` (which starts with `&`) and
/// returns its text and byte length. The bidi marks `&lrm;` and `&rlm;` are
/// dropped: the renderer lays text out on its own. An unknown `&` is literal.
fn entity(rest: &str) -> (String, usize) {
    for (name, ch) in [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&nbsp;", " "),
        ("&lrm;", ""),
        ("&rlm;", ""),
    ] {
        if rest.starts_with(name) {
            return (ch.to_owned(), name.len());
        }
    }
    if let Some(body) = rest.strip_prefix("&#") {
        if let Some(end) = body.find(';').filter(|&e| (1..=8).contains(&e)) {
            let num = &body[..end];
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse::<u32>().ok(),
            };
            if let Some(c) = code.and_then(char::from_u32).filter(|c| !c.is_control()) {
                return (c.to_string(), end + 3);
            }
        }
    }
    ("&".to_owned(), 1)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn srt_basic_with_italics_and_bom() {
        let srt = "\u{feff}1\r\n00:00:01,000 --> 00:00:03,500\r\nHello <i>there</i>\r\nsecond &amp; line\r\n\r\n2\r\n00:00:04,000 --> 00:00:05,000\r\n<i>Whole line</i>\r\n";
        let l = read(srt).unwrap().lines;
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].start_ms, l[0].end_ms), (1000, 3500));
        assert_eq!(l[0].text, "Hello there\nsecond & line");
        assert_eq!(l[0].raw, "Hello <i>there</i>\nsecond &amp; line");
        assert!(!l[0].italic);
        assert!(l[1].italic);
        assert_eq!(l[1].style, "Default");
        assert!(!l[1].typeset);
        assert_eq!(l[1].format, SourceFormat::Srt);
    }

    #[test]
    fn vtt_skips_header_notes_and_ids() {
        let vtt = "WEBVTT - title\n\nNOTE a comment\n\ncue-1\n00:01.000 --> 00:02.500 align:start\n<v Roger>Hi</v> <c.yellow>there</c>\n\n01:00:00.000 --> 01:00:01.000\n{\\i1}slanted{\\i0}\n";
        let l = read(vtt).unwrap().lines;
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].start_ms, l[0].end_ms), (1000, 2500));
        assert_eq!(l[0].text, "Hi there");
        assert_eq!(l[0].format, SourceFormat::Vtt);
        assert_eq!(l[1].start_ms, 3_600_000);
        assert!(l[1].italic);
    }

    #[test]
    fn a_bad_timestamp_skips_that_cue_and_counts_it() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nA\n\n2\n00:00:xx,000 --> 00:00:03,500\nB\n\n3\n00:00:04,000 --> 00:00:05,000\nC\n";
        let s = read(srt).unwrap();
        assert_eq!(s.skipped, 1);
        let texts: Vec<_> = s.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["A", "C"]);
    }

    #[test]
    fn text_with_no_cue_is_unrecognised() {
        assert_eq!(read("just some words\n"), Err(SourceError::Unrecognised));
        assert!(read("").unwrap().lines.is_empty());
        assert!(read("WEBVTT\n").unwrap().lines.is_empty());
    }

    #[test]
    fn cues_with_no_blank_line_between_them_stay_separate() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nA\n2\n00:00:03,000 --> 00:00:04,000\nB\nsecond\n00:00:05,000 --> 00:00:06,000\nC\n";
        let l = read(srt).unwrap().lines;
        let texts: Vec<_> = l.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["A", "B\nsecond", "C"]);
    }

    #[test]
    fn a_digit_line_is_text_unless_a_timing_line_follows() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n42\n";
        assert_eq!(read(srt).unwrap().lines[0].text, "42");
    }

    #[test]
    fn numeric_entities_decode_and_bidi_marks_drop() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nIt&#39;s &#x27;fine&#x27;&lrm; &rlm;&#0; &#xZZ; &copy;\n";
        assert_eq!(
            read(srt).unwrap().lines[0].text,
            "It's 'fine' &#0; &#xZZ; &copy;"
        );
    }

    const ASS_REORDERED: &str = "\u{feff}[Script Info]\r\nTitle: t\r\n\r\n[V4+ Styles]\r\nFormat: Name, Fontname\r\nStyle: Default,Arial\r\n\r\n[Events]\r\nFormat: Layer, Style, Start, End, Name, MarginL, MarginR, MarginV, Effect, Text\r\nDialogue: 0,Narr,0:00:01.00,0:00:02.50,,0,0,0,,Well, {\\i1}well{\\i0}, well\\Nagain\r\nComment: 0,Default,0:00:03.00,0:00:04.00,,0,0,0,,ignored\r\n";

    #[test]
    fn ass_format_reorder_commas_and_crlf() {
        let l = read(ASS_REORDERED).unwrap().lines;
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].start_ms, l[0].end_ms), (1000, 2500));
        assert_eq!(l[0].style, "Narr");
        assert_eq!(l[0].text, "Well, well, well\nagain");
        assert_eq!(l[0].raw, "Well, {\\i1}well{\\i0}, well\\Nagain");
        assert!(!l[0].italic);
        assert!(!l[0].typeset);
    }

    #[test]
    fn ass_whole_line_italic() {
        let ass = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}All slanted\n";
        let l = read(ass).unwrap().lines;
        assert!(l[0].italic);
    }

    #[test]
    fn ass_karaoke_syllables() {
        let ass = "[Events]\nDialogue: 0,0:00:10.00,0:00:12.00,Song,,0,0,0,,{\\k25}ka{\\kf50}ra{\\ko10}o{\\K5}ke\n";
        let l = read(ass).unwrap().lines;
        let k = l[0].karaoke.as_ref().unwrap();
        let got: Vec<_> = k.iter().map(|s| (s.dur_ms, s.text.as_str())).collect();
        assert_eq!(got, [(250, "ka"), (500, "ra"), (100, "o"), (50, "ke")]);
        assert_eq!(l[0].text, "karaoke");
    }

    #[test]
    fn ass_leading_text_before_first_k_is_a_zero_syllable() {
        let ass = "[Events]\nDialogue: 0,0:00:10.00,0:00:12.00,Song,,0,0,0,,la {\\k20}la\n";
        let k = read(ass).unwrap().lines[0].karaoke.clone().unwrap();
        assert_eq!((k[0].dur_ms, k[0].text.as_str()), (0, "la "));
        assert_eq!((k[1].dur_ms, k[1].text.as_str()), (200, "la"));
    }

    #[test]
    fn ass_typeset_detection() {
        let mk = |text: &str| {
            let ass = format!("[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Sign,,0,0,0,,{text}\n");
            read(&ass).unwrap().lines.remove(0)
        };
        assert!(mk(r"{\pos(100,200)}Baratie").typeset);
        assert!(mk(r"{\an8}Top").typeset);
        assert!(mk(r"{\move(0,0,10,10)}x").typeset);
        assert!(mk(r"{\clip(0,0,5,5)}x").typeset);
        assert!(mk(r"{\p1}m 0 0 l 5 5{\p0}").typeset);
        assert!(!mk(r"{\an2}bottom").typeset);
        assert!(mk(r"{\a6}legacy top").typeset);
        assert!(!mk(r"{\a2}legacy bottom").typeset);
        assert!(!mk(r"{\fad(100,100)\i1}plain").typeset);
        assert_eq!(mk(r"{\p1}m 0 0 l 5 5{\p0}after").text, "after");
        // libass reads a tag's leading digits, so a stray character after
        // them still starts a drawing (B1208).
        let stray = mk(r"{\bord0\p1)}m 0 0 l 510 0 510 420 0 420");
        assert!(stray.typeset);
        assert_eq!(stray.text, "");
    }

    /// B1208: a line whose style sits anywhere but bottom centre is a sign,
    /// unless the line puts itself back at bottom centre.
    #[test]
    fn a_style_placed_off_bottom_centre_makes_a_sign() {
        let ass = "[V4+ Styles]\n\
            Format: Name, Fontname, Fontsize, Alignment, MarginV\n\
            Style: Menu,Times New Roman,36,7,90\n\
            Style: Default,Arial,48,2,30\n\
            Style: Odd,Arial,48,x,30\n\
            [Events]\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Menu,,0,0,0,,Stats\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Menu,,0,0,0,,{\\an2}Stats at the bottom\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hello\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Odd,,0,0,0,,Hello\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Missing,,0,0,0,,Hello\n";
        let l = read(ass).unwrap().lines;
        let flags: Vec<bool> = l.iter().map(|l| l.typeset).collect();
        assert_eq!(flags, [true, false, false, false, false]);
        assert_eq!(
            l[0].style_def.as_deref().and_then(SourceStyle::alignment),
            Some(7)
        );
    }

    #[test]
    fn a_short_dialogue_is_skipped_and_counted() {
        let ass = "[Events]\nDialogue: 0,0:00:01.00\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,ok\n";
        let s = read(ass).unwrap();
        assert_eq!((s.lines.len(), s.skipped), (1, 1));
        assert!(matches!(
            ass_events(ass),
            Err(SourceError::BadEvent { line: 2 })
        ));
    }

    #[test]
    fn styles_are_kept_verbatim_by_name() {
        let ass = "[V4+ Styles]\nFormat: Name, Fontname, Fontsize, Alignment\nStyle: Sign,Impact,90,8\n\n[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Sign,,0,0,0,,{\\pos(3,2)}X\nDialogue: 0,0:00:03.00,0:00:04.00,Nope,,0,0,0,,Y\n";
        let l = read(ass).unwrap().lines;
        let d = l[0].style_def.as_ref().unwrap();
        assert_eq!(d.line, "Style: Sign,Impact,90,8");
        assert_eq!(d.format, "Format: Name, Fontname, Fontsize, Alignment");
        assert!(l[1].style_def.is_none());
    }

    #[test]
    fn time_formats() {
        assert_eq!(parse_time("0:01:02.50"), Some(62_500));
        assert_eq!(parse_time("01:02:03,004"), Some(3_723_004));
        assert_eq!(parse_time("02:03.5"), Some(123_500));
        assert_eq!(parse_time("nope"), None);
        assert_eq!(parse_time("99999999999999999:00:01,000"), None);
        assert_eq!(parse_time("-1:00:00"), None);
        assert_eq!(parse_time("0:+1:00.00"), None);
        assert_eq!(parse_time("0:00:01.1234"), None);
        assert_eq!(parse_time("100:00:00.00"), None);
        assert_eq!(parse_time("0:99:99.00"), Some(6_039_000));
    }
}
