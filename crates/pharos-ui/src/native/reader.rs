//! The book reader's rules (spec 023): its settings and how they are kept,
//! the size cap on weak devices, when a position is saved, what each key
//! does and how progress reads. No I/O; `reader_web.rs` draws it.

use quire::{Locator, TocEntry};
use serde::{Deserialize, Serialize};

use super::playback::DeviceClass;

/// Where the settings are kept on this device (spec 023 US2).
pub const SETTINGS_KEY: &str = "pharos.reader.settings";

/// The largest book a TV, headset or phone opens. The whole book is read
/// into wasm memory, which never shrinks, and a TV has about 2 GB for the
/// browser and everything else.
pub const WEAK_DEVICE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A position is saved at most this often while reading (FR-006).
pub const SAVE_EVERY_MS: f64 = 5_000.0;

/// The font sizes the steps move through, in px.
pub const FONT_SIZES: [u32; 9] = [14, 16, 18, 20, 22, 26, 30, 36, 44];
/// Unitless line heights, times ten.
pub const LINE_HEIGHTS: [u32; 4] = [13, 15, 17, 20];
/// Space around the text, in px.
pub const MARGINS: [u32; 4] = [12, 40, 72, 120];

/// A fraction at or past which the book counts as finished.
const FINISHED: f64 = 0.99;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Typeface {
    #[default]
    Book,
    Serif,
    Sans,
}

impl Typeface {
    pub const ALL: [Typeface; 3] = [Typeface::Book, Typeface::Serif, Typeface::Sans];

    pub fn label(self) -> &'static str {
        match self {
            Typeface::Book => "Book",
            Typeface::Serif => "Serif",
            Typeface::Sans => "Sans",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageTheme {
    /// The pharos theme's own colours.
    #[default]
    Pharos,
    Paper,
    Sepia,
    Night,
}

impl PageTheme {
    pub const ALL: [PageTheme; 4] = [
        PageTheme::Pharos,
        PageTheme::Paper,
        PageTheme::Sepia,
        PageTheme::Night,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PageTheme::Pharos => "Pharos",
            PageTheme::Paper => "Paper",
            PageTheme::Sepia => "Sepia",
            PageTheme::Night => "Night",
        }
    }
}

/// The reader's own settings, kept per device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReaderSettings {
    pub font_px: u32,
    /// Line height times ten (15 is 1.5).
    pub line_height_x10: u32,
    pub margin_px: u32,
    pub typeface: Typeface,
    pub theme: PageTheme,
}

impl Default for ReaderSettings {
    fn default() -> Self {
        Self {
            font_px: 18,
            line_height_x10: 15,
            margin_px: 40,
            typeface: Typeface::Book,
            theme: PageTheme::Pharos,
        }
    }
}

/// The step of `steps` nearest to `value`.
fn snap(steps: &[u32], value: u32) -> u32 {
    steps
        .iter()
        .copied()
        .min_by_key(|s| s.abs_diff(value))
        .unwrap_or(value)
}

/// The step `by` places from the one nearest `value`, held at either end.
fn step(steps: &[u32], value: u32, by: isize) -> u32 {
    let at = steps
        .iter()
        .position(|s| *s == snap(steps, value))
        .unwrap_or(0);
    let to = at
        .saturating_add_signed(by)
        .min(steps.len().saturating_sub(1));
    steps.get(to).copied().unwrap_or(value)
}

impl ReaderSettings {
    /// The settings stored as `text`; anything unreadable is the defaults,
    /// and numbers off the steps move to the nearest one.
    pub fn parse(text: &str) -> Self {
        let mut s: Self = serde_json::from_str(text).unwrap_or_default();
        s.font_px = snap(&FONT_SIZES, s.font_px);
        s.line_height_x10 = snap(&LINE_HEIGHTS, s.line_height_x10);
        s.margin_px = snap(&MARGINS, s.margin_px);
        s
    }

    /// The text to keep.
    pub fn to_text(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }

    /// One font step larger (`by` 1) or smaller (-1).
    pub fn font_step(self, by: isize) -> Self {
        Self {
            font_px: step(&FONT_SIZES, self.font_px, by),
            ..self
        }
    }

    pub fn can_step_font(self, by: isize) -> bool {
        step(&FONT_SIZES, self.font_px, by) != self.font_px
    }

    pub fn line_height(self) -> f64 {
        f64::from(self.line_height_x10) / 10.0
    }

    /// The next line height, round to the first after the last.
    pub fn next_line_height(self) -> Self {
        Self {
            line_height_x10: cycle(&LINE_HEIGHTS, self.line_height_x10),
            ..self
        }
    }

    /// The next margin, round to the first after the last.
    pub fn next_margin(self) -> Self {
        Self {
            margin_px: cycle(&MARGINS, self.margin_px),
            ..self
        }
    }
}

fn cycle(steps: &[u32], value: u32) -> u32 {
    let at = steps.iter().position(|s| *s == value).unwrap_or(0);
    steps
        .get(at.saturating_add(1) % steps.len().max(1))
        .copied()
        .unwrap_or(value)
}

/// Whether the page is a weak device (a TV, a headset or a phone), whose
/// memory a whole book can fill.
pub fn weak_device(class: DeviceClass, user_agent: &str) -> bool {
    class != DeviceClass::Desktop
        || user_agent.contains("OculusBrowser")
        || user_agent.contains("Quest")
}

/// The largest book this device opens; `None` is no cap.
pub fn size_cap(class: DeviceClass, user_agent: &str) -> Option<u64> {
    weak_device(class, user_agent).then_some(WEAK_DEVICE_MAX_BYTES)
}

/// What the book's size allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeVerdict {
    Open,
    /// No `Content-Length` came back, so the size is unknown. Opened and
    /// logged rather than refused: a proxy that compresses drops the header,
    /// and refusing would lock every book out of that device.
    OpenUnknown,
    TooBig {
        bytes: u64,
        cap: u64,
    },
}

pub fn check_size(content_length: Option<u64>, cap: Option<u64>) -> SizeVerdict {
    match (content_length, cap) {
        (_, None) => SizeVerdict::Open,
        (None, Some(_)) => SizeVerdict::OpenUnknown,
        (Some(bytes), Some(cap)) if bytes > cap => SizeVerdict::TooBig { bytes, cap },
        (Some(_), Some(_)) => SizeVerdict::Open,
    }
}

/// The line shown when a book is over the cap.
pub fn too_big_message(bytes: u64, cap: u64) -> String {
    format!(
        "This book is {} MB, more than the {} MB this device can open here. Download it to read elsewhere.",
        mb(bytes),
        mb(cap)
    )
}

fn mb(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

/// When a position is sent to the server.
#[derive(Debug, Clone, Default)]
pub struct SaveGate {
    last_sent_at: Option<f64>,
    last_sent: Option<String>,
}

impl SaveGate {
    /// Whether to send `locator` now. A forced save (the reader leaving, the
    /// page hidden) goes unless the server already has this place; a
    /// passing one also waits [`SAVE_EVERY_MS`] after the last. Between
    /// those there is no timer: the next page turn asks again, so a reader
    /// who stops loses at most the time since their last save.
    pub fn due(&self, locator: &str, now_ms: f64, forced: bool) -> bool {
        if self.last_sent.as_deref() == Some(locator) {
            return false;
        }
        forced
            || self
                .last_sent_at
                .map_or(true, |at| now_ms - at >= SAVE_EVERY_MS)
    }

    pub fn sent(&mut self, locator: &str, now_ms: f64) {
        self.last_sent_at = Some(now_ms);
        self.last_sent = Some(locator.to_owned());
    }
}

/// What a page-turn key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Next,
    Prev,
    /// Escape: close the open panel, else leave.
    Escape,
}

/// The reader's meaning of `key`; Ctrl and Cmd presses are the browser's.
pub fn key_action(key: &str, ctrl: bool, meta: bool) -> Option<KeyAction> {
    if ctrl || meta {
        return None;
    }
    match key {
        "ArrowRight" | "PageDown" | " " => Some(KeyAction::Next),
        "ArrowLeft" | "PageUp" => Some(KeyAction::Prev),
        "Escape" => Some(KeyAction::Escape),
        _ => None,
    }
}

/// `fraction` of the book read as a whole percent, 0 to 100.
pub fn percent(fraction: f64) -> u32 {
    if !fraction.is_finite() {
        return 0;
    }
    (fraction.clamp(0.0, 1.0) * 100.0).floor() as u32
}

/// The progress line's text, such as `42%`.
pub fn percent_label(fraction: f64) -> String {
    format!("{}%", percent(fraction))
}

/// The title page's button for a book with a saved place: part read shows
/// how far, never 0% or 100%.
pub fn continue_label(fraction: f32) -> Option<String> {
    let f = f64::from(fraction);
    (f.is_finite() && f > 0.0).then(|| format!("Continue reading · {}%", percent(f).clamp(1, 99),))
}

/// The fraction to store: the end of the book reads as 1.
pub fn saved_fraction(progress: f64, at_end: bool) -> f32 {
    if at_end {
        return 1.0;
    }
    let p = if progress.is_finite() { progress } else { 0.0 };
    p.clamp(0.0, FINISHED_BELOW) as f32
}

/// Progress short of the last page stays under the finished mark, so only
/// reaching the end marks a book read.
const FINISHED_BELOW: f64 = FINISHED - 0.001;

/// The saved locator, if it is one this reader wrote.
pub fn saved_locator(text: &str) -> Option<Locator> {
    text.parse().ok()
}

/// One line of the table of contents panel.
#[derive(Debug, Clone, PartialEq)]
pub struct TocRow {
    pub depth: usize,
    pub title: String,
    /// `None` for a heading with no link of its own.
    pub target: Option<quire::Target>,
}

/// The table of contents as one list, children after their parent.
pub fn flatten_toc(entries: &[TocEntry]) -> Vec<TocRow> {
    fn walk(entries: &[TocEntry], depth: usize, out: &mut Vec<TocRow>) {
        for e in entries {
            out.push(TocRow {
                depth,
                title: e.title.trim().to_owned(),
                target: e.target.clone(),
            });
            walk(&e.children, depth.saturating_add(1), out);
        }
    }
    let mut out = Vec::new();
    walk(entries, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use quire::Target;

    #[test]
    fn settings_round_trip_through_their_text() {
        let s = ReaderSettings {
            font_px: 26,
            line_height_x10: 20,
            margin_px: 72,
            typeface: Typeface::Serif,
            theme: PageTheme::Night,
        };
        assert_eq!(ReaderSettings::parse(&s.to_text()), s);
    }

    #[test]
    fn unreadable_settings_are_the_defaults() {
        for text in ["", "nope", "[]", "{\"font_px\":"] {
            assert_eq!(ReaderSettings::parse(text), ReaderSettings::default());
        }
    }

    #[test]
    fn stored_numbers_snap_to_a_step_and_missing_ones_default() {
        let s = ReaderSettings::parse(r#"{"font_px":9000,"margin_px":0,"theme":"sepia"}"#);
        assert_eq!(s.font_px, 44);
        assert_eq!(s.margin_px, 12);
        assert_eq!(s.theme, PageTheme::Sepia);
        assert_eq!(s.line_height_x10, 15);
        // A theme this build lacks makes the whole value unreadable: defaults.
        let t = ReaderSettings::parse(r#"{"theme":"neon","font_px":22}"#);
        assert_eq!(t, ReaderSettings::default());
    }

    #[test]
    fn font_steps_stop_at_either_end() {
        let mut s = ReaderSettings::default();
        for _ in 0..20 {
            s = s.font_step(1);
        }
        assert_eq!(s.font_px, 44);
        assert!(!s.can_step_font(1));
        assert!(s.can_step_font(-1));
        for _ in 0..20 {
            s = s.font_step(-1);
        }
        assert_eq!(s.font_px, 14);
        assert!(!s.can_step_font(-1));
        assert_eq!(ReaderSettings::default().font_step(1).font_px, 20);
    }

    #[test]
    fn line_height_and_margin_cycle_through_their_steps() {
        let mut s = ReaderSettings::default();
        let mut seen = vec![s.line_height_x10];
        for _ in 0..LINE_HEIGHTS.len() {
            s = s.next_line_height();
            seen.push(s.line_height_x10);
        }
        assert_eq!(seen, [15, 17, 20, 13, 15]);
        assert!((ReaderSettings::default().line_height() - 1.5).abs() < f64::EPSILON);
        assert_eq!(ReaderSettings::default().next_margin().margin_px, 72);
    }

    #[test]
    fn only_weak_devices_are_capped() {
        let desktop = "Mozilla/5.0 (X11; Linux x86_64) Chrome/130";
        assert_eq!(size_cap(DeviceClass::Desktop, desktop), None);
        assert_eq!(
            size_cap(DeviceClass::Tv, desktop),
            Some(WEAK_DEVICE_MAX_BYTES)
        );
        assert_eq!(
            size_cap(DeviceClass::Phone, desktop),
            Some(WEAK_DEVICE_MAX_BYTES)
        );
        // A headset reports as a desktop; its agent says otherwise.
        for ua in [
            "Mozilla/5.0 OculusBrowser/38.0 Chrome/130",
            "Mozilla/5.0 (Quest 3)",
        ] {
            assert_eq!(
                size_cap(DeviceClass::Desktop, ua),
                Some(WEAK_DEVICE_MAX_BYTES)
            );
        }
    }

    #[test]
    fn a_book_over_the_cap_is_refused_and_an_unknown_size_is_not() {
        let cap = Some(WEAK_DEVICE_MAX_BYTES);
        assert_eq!(
            check_size(Some(WEAK_DEVICE_MAX_BYTES), cap),
            SizeVerdict::Open
        );
        assert_eq!(
            check_size(Some(WEAK_DEVICE_MAX_BYTES + 1), cap),
            SizeVerdict::TooBig {
                bytes: WEAK_DEVICE_MAX_BYTES + 1,
                cap: WEAK_DEVICE_MAX_BYTES
            }
        );
        assert_eq!(check_size(None, cap), SizeVerdict::OpenUnknown);
        assert_eq!(check_size(Some(u64::MAX), None), SizeVerdict::Open);
        assert_eq!(check_size(None, None), SizeVerdict::Open);
        assert!(too_big_message(214 * 1024 * 1024, WEAK_DEVICE_MAX_BYTES).contains("214 MB"));
    }

    #[test]
    fn saves_wait_five_seconds_but_leaving_does_not() {
        let mut gate = SaveGate::default();
        assert!(gate.due("epub1:0:0", 1_000.0, false), "the first goes");
        gate.sent("epub1:0:0", 1_000.0);
        assert!(!gate.due("epub1:0:10", 3_000.0, false), "too soon");
        assert!(gate.due("epub1:0:10", 6_000.0, false), "5 s on");
        assert!(gate.due("epub1:0:10", 3_000.0, true), "leaving is forced");
        assert!(
            !gate.due("epub1:0:0", 60_000.0, true),
            "the server has this place already"
        );
    }

    #[test]
    fn keys_turn_pages() {
        for k in ["ArrowRight", "PageDown", " "] {
            assert_eq!(key_action(k, false, false), Some(KeyAction::Next), "{k}");
        }
        for k in ["ArrowLeft", "PageUp"] {
            assert_eq!(key_action(k, false, false), Some(KeyAction::Prev), "{k}");
        }
        assert_eq!(key_action("Escape", false, false), Some(KeyAction::Escape));
        assert_eq!(key_action("ArrowRight", true, false), None);
        assert_eq!(key_action("ArrowRight", false, true), None);
        assert_eq!(key_action("a", false, false), None);
    }

    #[test]
    fn progress_reads_as_a_whole_percent() {
        assert_eq!(percent_label(0.0), "0%");
        assert_eq!(percent_label(0.4279), "42%");
        assert_eq!(percent_label(1.0), "100%");
        assert_eq!(percent_label(7.0), "100%");
        assert_eq!(percent_label(-1.0), "0%");
        assert_eq!(percent_label(f64::NAN), "0%");
    }

    #[test]
    fn a_part_read_book_says_how_far() {
        assert_eq!(continue_label(0.0), None);
        assert_eq!(continue_label(f32::NAN), None);
        assert_eq!(
            continue_label(0.426).as_deref(),
            Some("Continue reading · 42%")
        );
        assert_eq!(
            continue_label(0.001).as_deref(),
            Some("Continue reading · 1%")
        );
        assert_eq!(
            continue_label(0.995).as_deref(),
            Some("Continue reading · 99%")
        );
    }

    #[test]
    fn only_the_end_of_the_book_marks_it_read() {
        assert!(f64::from(saved_fraction(0.9999, false)) < FINISHED);
        assert!((saved_fraction(0.5, false) - 0.5).abs() < f32::EPSILON);
        assert!((saved_fraction(0.1, true) - 1.0).abs() < f32::EPSILON);
        assert!((saved_fraction(f64::NAN, false)).abs() < f32::EPSILON);
    }

    #[test]
    fn only_a_quire_locator_is_restored() {
        assert_eq!(
            saved_locator("epub1:3:1200"),
            Some(Locator {
                spine: 3,
                offset: 1200
            })
        );
        assert_eq!(saved_locator("pdf1:3"), None);
        assert_eq!(saved_locator(""), None);
    }

    #[test]
    fn the_table_of_contents_flattens_in_reading_order() {
        let entry = |title: &str, spine: usize, children: Vec<TocEntry>| TocEntry {
            title: format!(" {title} "),
            target: Some(Target {
                spine,
                fragment: None,
            }),
            children,
        };
        let toc = vec![
            entry("One", 0, vec![entry("One A", 1, vec![])]),
            TocEntry {
                title: "Part".into(),
                target: None,
                children: vec![entry("Two", 2, vec![])],
            },
        ];
        let rows = flatten_toc(&toc);
        let shape: Vec<(usize, &str, Option<usize>)> = rows
            .iter()
            .map(|r| {
                (
                    r.depth,
                    r.title.as_str(),
                    r.target.as_ref().map(|t| t.spine),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (0, "One", Some(0)),
                (1, "One A", Some(1)),
                (0, "Part", None),
                (1, "Two", Some(2)),
            ]
        );
    }
}
