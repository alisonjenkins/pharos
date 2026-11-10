//! The book reader's rules (spec 023): its settings and how they are kept,
//! the size cap on weak devices, when a position is saved, what each key
//! does and how progress reads. No I/O; `reader_web.rs` draws it.

use std::collections::BTreeMap;

use pharos_client::api::{Book, BookFormat, ItemRef, ReadPosition};
use pharos_client::ClientError;
use quire::{page_progress, Locator, PageLocator, TocEntry};
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

/// A PDF page's backing store is drawn at the device's pixel ratio up to
/// this much. Beyond it the extra pixels cost render time and memory on a
/// headset or TV for detail the eye cannot use at reading distance.
pub const PDF_MAX_PIXEL_RATIO: f64 = 2.0;

/// The longest side, in pixels, of a PDF page's backing store. A 4096 px
/// canvas is the largest a weak GPU is sure to take, and the render time
/// grows with the pixels.
pub const PDF_MAX_SIDE_PX: u32 = 4096;

/// A swipe shorter than this, in px, is a tap (quire-web's value, so a PDF
/// and an EPUB feel alike).
pub const SWIPE_PX: f64 = 40.0;

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

/// How a PDF page sits in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfFit {
    /// The whole page is visible.
    #[default]
    Page,
    /// The page is as wide as the window, and scrolls up and down.
    Width,
}

impl PdfFit {
    pub fn label(self) -> &'static str {
        match self {
            PdfFit::Page => "Fit page",
            PdfFit::Width => "Fit width",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            PdfFit::Page => PdfFit::Width,
            PdfFit::Width => PdfFit::Page,
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
    pub pdf_fit: PdfFit,
}

impl Default for ReaderSettings {
    fn default() -> Self {
        Self {
            font_px: 18,
            line_height_x10: 15,
            margin_px: 40,
            typeface: Typeface::Book,
            theme: PageTheme::Pharos,
            pdf_fit: PdfFit::Page,
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
}

/// A name for a margin step.
pub fn margin_label(px: u32) -> &'static str {
    match px {
        0..=20 => "None",
        21..=50 => "Narrow",
        51..=90 => "Medium",
        _ => "Wide",
    }
}

/// What the save gate compares: a place and how far through it is, so
/// reaching the end of the book is sent even from a place already saved.
pub fn save_key(locator: &str, fraction: f32) -> String {
    format!("{locator}@{fraction:.3}")
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

/// Where saves the server has not answered are kept on this device (spec 036
/// FR-001): a map of item id to the newest one for that book.
pub const PENDING_KEY: &str = "pharos.reader.pending";

/// The wait before the first retry of a queued save, doubling each time.
pub const RETRY_FIRST_MS: u32 = 2_000;

/// The longest wait between retries: a tunnel is minutes, and a retry is one
/// small call.
pub const RETRY_MAX_MS: u32 = 60_000;

/// Books kept in the queue; the oldest reading goes first. Bounds what
/// `localStorage` holds if a device stays offline across many books.
pub const PENDING_MAX_ENTRIES: usize = 50;

/// The wait before retry number `attempt` (0 is the first retry).
pub fn retry_delay_ms(attempt: u32) -> u32 {
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    RETRY_FIRST_MS.saturating_mul(factor).min(RETRY_MAX_MS)
}

/// Whether a failed save is worth keeping to send again: the connection
/// failed, the server was unavailable or rate limiting, or the session needs
/// signing in again (the entry is held and sent after sign-in). Only a
/// refusal (invalid, not found, not permitted) would fail the same way
/// every time.
pub fn retryable(error: &ClientError) -> bool {
    match error {
        ClientError::Rpc(_) => !error.is_unimplemented(),
        ClientError::Api { kind, .. } => retryable_kind(kind),
    }
}

/// [`retryable`] for a server `Error.kind`.
pub fn retryable_kind(kind: &str) -> bool {
    matches!(kind, "unavailable" | "limit" | "revoked")
}

/// A place in a book and when the reader got there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingSave {
    pub locator: String,
    pub fraction: f32,
    /// Unix milliseconds on this device's clock: when the page was read.
    pub read_at_ms: i64,
}

/// The saves waiting for the server, one per book.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingSaves(BTreeMap<String, PendingSave>);

impl PendingSaves {
    /// The queue as kept; unreadable text is an empty queue (a lost queue
    /// costs a position, a crash costs the reader).
    pub fn parse(text: &str) -> Self {
        Self::parse_counting(text).0
    }

    /// [`PendingSaves::parse`], also answering how many entries were dropped
    /// because their key is not an item id (corrupt storage): they could
    /// never be sent and would keep the retry loop and the note alive.
    pub fn parse_counting(text: &str) -> (Self, usize) {
        let all: BTreeMap<String, PendingSave> = serde_json::from_str(text).unwrap_or_default();
        let total = all.len();
        let kept: BTreeMap<String, PendingSave> = all
            .into_iter()
            .filter(|(k, _)| k.parse::<ItemRef>().is_ok())
            .collect();
        let dropped = total.saturating_sub(kept.len());
        (Self(kept), dropped)
    }

    pub fn to_text(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, id: ItemRef) -> bool {
        self.0.contains_key(&id.to_string())
    }

    /// Keeps `save` for `id` unless one read later is already queued for it.
    /// Over [`PENDING_MAX_ENTRIES`] the book read longest ago is dropped.
    pub fn queue(&mut self, id: ItemRef, save: PendingSave) {
        let key = id.to_string();
        if self
            .0
            .get(&key)
            .is_some_and(|k| k.read_at_ms > save.read_at_ms)
        {
            return;
        }
        self.0.insert(key, save);
        while self.0.len() > PENDING_MAX_ENTRIES {
            let oldest = self
                .0
                .iter()
                .min_by_key(|(_, v)| v.read_at_ms)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.0.remove(&k),
                None => break,
            };
        }
    }

    /// Forgets the save for `id` once the server has answered it, unless a
    /// different one was queued meanwhile. Answers whether it was removed.
    pub fn answered(&mut self, id: ItemRef, sent: &PendingSave) -> bool {
        let key = id.to_string();
        if self.0.get(&key) == Some(sent) {
            self.0.remove(&key);
            return true;
        }
        false
    }

    /// Every save to send, oldest reading first; entries whose key is not an
    /// item id are dropped.
    pub fn entries(&self) -> Vec<(ItemRef, PendingSave)> {
        let mut all: Vec<_> = self
            .0
            .iter()
            .filter_map(|(k, v)| Some((k.parse::<ItemRef>().ok()?, v.clone())))
            .collect();
        all.sort_by_key(|(_, v)| v.read_at_ms);
        all
    }
}

/// The note shown while a save waits for the connection.
pub const OFFLINE_NOTE: &str = "Offline \u{b7} your place is saved on this device";

/// What a page-turn key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Next,
    Prev,
    /// Escape: close the open panel, else leave.
    Escape,
}

/// What a key press came from, as far as the reader cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTarget {
    /// The page, or nothing in particular.
    Page,
    /// A button or link, which takes Space (and Enter) to activate itself.
    Control,
    /// A text field or select, which takes every key for itself.
    Field,
}

/// A key press and its modifiers.
#[derive(Debug, Clone, Copy)]
pub struct KeyPress<'a> {
    pub key: &'a str,
    pub ctrl: bool,
    pub meta: bool,
    pub alt: bool,
    pub shift: bool,
    pub target: KeyTarget,
}

/// The reader's meaning of a press. Ctrl, Cmd and Alt presses are the
/// browser's (Alt+Arrow is back and forward), and a control keeps the keys
/// that activate it so the toolbar works from the keyboard.
pub fn key_action(press: KeyPress<'_>) -> Option<KeyAction> {
    if press.ctrl || press.meta || press.alt || press.target == KeyTarget::Field {
        return None;
    }
    match press.key {
        "ArrowRight" | "PageDown" => Some(KeyAction::Next),
        "ArrowLeft" | "PageUp" => Some(KeyAction::Prev),
        " " if press.target == KeyTarget::Control => None,
        " " if press.shift => Some(KeyAction::Prev),
        " " => Some(KeyAction::Next),
        "Escape" => Some(KeyAction::Escape),
        _ => None,
    }
}

/// How far Space, Page Up and Down and the vertical arrows scroll a fit-width
/// page, as a share of the host's height (a little under a screenful, so a
/// line stays in view across the jump).
pub const PDF_SCROLL_STEP: f64 = 0.9;

/// A scroll position within a tolerance of this many px counts as the edge.
const SCROLL_EDGE_PX: f64 = 1.0;

/// Where the PDF host is scrolled, in CSS px.
#[derive(Debug, Clone, Copy)]
pub struct HostScroll {
    pub top: f64,
    pub client_height: f64,
    pub scroll_height: f64,
}

/// What a key does to a PDF page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PdfKey {
    /// Turn to the next (`true`) or previous page.
    Turn(bool),
    /// Scroll the page by this many px (negative is up).
    Scroll(f64),
}

/// The meaning of a press on a PDF page. A fit-page page only turns. A
/// fit-width page is taller than the window, so Space, Page Up and Down and
/// the vertical arrows scroll it and turn only once it is already at the
/// end (or the start, going back); otherwise the lower part of a tall page
/// could not be reached from a keyboard or a TV remote. Left and Right still
/// turn. Escape is the caller's.
pub fn pdf_key(press: KeyPress<'_>, fit: PdfFit, scroll: HostScroll) -> Option<PdfKey> {
    let action = match (press.key, fit) {
        // The vertical arrows mean the page keys, with the same modifier and
        // focus rules, only where the page scrolls.
        ("ArrowDown", PdfFit::Width) => key_action(KeyPress {
            key: "PageDown",
            ..press
        }),
        ("ArrowUp", PdfFit::Width) => key_action(KeyPress {
            key: "PageUp",
            ..press
        }),
        _ => key_action(press),
    }?;
    let forward = match action {
        KeyAction::Next => true,
        KeyAction::Prev => false,
        KeyAction::Escape => return None,
    };
    let scrolls = fit == PdfFit::Width
        && matches!(
            press.key,
            " " | "PageDown" | "PageUp" | "ArrowDown" | "ArrowUp"
        );
    let known = [scroll.top, scroll.client_height, scroll.scroll_height]
        .iter()
        .all(|v| v.is_finite());
    if !scrolls || !known {
        return Some(PdfKey::Turn(forward));
    }
    let at_end = scroll.top + scroll.client_height >= scroll.scroll_height - SCROLL_EDGE_PX;
    let at_start = scroll.top <= SCROLL_EDGE_PX;
    let step = scroll.client_height * PDF_SCROLL_STEP;
    Some(match (forward, at_end, at_start) {
        (true, true, _) | (false, _, true) => PdfKey::Turn(forward),
        (true, false, _) => PdfKey::Scroll(step),
        (false, _, false) => PdfKey::Scroll(-step),
    })
}

/// `fraction` of the book read as a whole percent, 0 to 100.
pub fn percent(fraction: f64) -> u32 {
    if !fraction.is_finite() {
        return 0;
    }
    // The nudge keeps 0.42 stored as an f32 (0.41999998) from reading 41.
    (fraction.clamp(0.0, 1.0) * 100.0 + 1e-4).floor() as u32
}

/// The progress line's text, such as `42%`.
pub fn percent_label(fraction: f64) -> String {
    format!("{}%", percent(fraction))
}

/// The progress line's text: for a PDF the page too, such as `Page 3 of 12 ·
/// 25%`. `page` is the page index (from 0) and the page count.
pub fn progress_label(fraction: f64, page: Option<(usize, usize)>) -> String {
    match page {
        Some((index, count)) => format!(
            "Page {} of {count} · {}",
            index.saturating_add(1),
            percent_label(fraction)
        ),
        None => percent_label(fraction),
    }
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

/// Where a book's bytes are, for the reader and for the Download link.
pub fn download_href(id: ItemRef) -> String {
    format!("/Items/{id}/Download")
}

/// What a book's title page offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookAction {
    /// A Read button with this label.
    Read(String),
    /// No Read button, and this line says why (V353).
    Closed(&'static str),
}

pub fn book_action(book: &Book, position: Option<&ReadPosition>) -> BookAction {
    match book.format {
        BookFormat::Epub | BookFormat::Pdf => BookAction::Read(
            position
                .and_then(|p| continue_label(p.fraction))
                .unwrap_or_else(|| "Read".to_owned()),
        ),
        BookFormat::Unreadable => {
            BookAction::Closed("This book is locked to a Kindle device and cannot be opened here.")
        }
        BookFormat::Comic => BookAction::Closed("Comic reading is coming."),
    }
}

/// The saved locator, if it is one this reader wrote.
pub fn saved_locator(text: &str) -> Option<Locator> {
    text.parse().ok()
}

/// Where a PDF page is drawn and how many pixels it takes (spec 023 US5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfLayout {
    pub css_width: f64,
    pub css_height: f64,
    /// The height to ask the engine for; the width follows the page.
    pub render_height: u32,
}

/// The size of a page whose width over height is `aspect`, in a window
/// `viewport_w` by `viewport_h` CSS px, on a screen of `pixel_ratio`. A
/// window or page with no size is treated as 1 px, never zero.
pub fn pdf_layout(
    viewport_w: f64,
    viewport_h: f64,
    aspect: f64,
    fit: PdfFit,
    pixel_ratio: f64,
) -> PdfLayout {
    let positive = |v: f64, or: f64| if v.is_finite() && v > 0.0 { v } else { or };
    let (vw, vh) = (positive(viewport_w, 1.0), positive(viewport_h, 1.0));
    let aspect = positive(aspect, 1.0);
    let css_height = match fit {
        PdfFit::Page => vh.min(vw / aspect),
        PdfFit::Width => vw / aspect,
    };
    let css_width = css_height * aspect;
    let ratio = positive(pixel_ratio, 1.0).min(PDF_MAX_PIXEL_RATIO);
    // Neither side of the backing store passes the cap, so a very wide page
    // is drawn shorter, not wider.
    let cap = f64::from(PDF_MAX_SIDE_PX).min(f64::from(PDF_MAX_SIDE_PX) / aspect);
    let render_height = (css_height * ratio).min(cap).round().max(1.0) as u32;
    PdfLayout {
        css_width,
        css_height,
        render_height,
    }
}

/// The page index a turn reaches from `index` in a book of `count` pages,
/// or `None` at either end.
pub fn turn_to(index: usize, count: usize, forward: bool) -> Option<usize> {
    if forward {
        let next = index.checked_add(1)?;
        (next < count).then_some(next)
    } else {
        index.checked_sub(1)
    }
}

/// The one page worth drawing ahead of time: the next. A page turned back to
/// is drawn when asked for, so one spare canvas is all the reader holds.
pub fn prerender_index(index: usize, count: usize) -> Option<usize> {
    turn_to(index, count, true)
}

/// What a click `x` (0 at the page's left edge, 1 at its right) asks for: the
/// outer thirds turn the page, the middle does nothing.
pub fn tap_action(x: f64) -> Option<KeyAction> {
    if x < 1.0 / 3.0 {
        Some(KeyAction::Prev)
    } else if x > 2.0 / 3.0 {
        Some(KeyAction::Next)
    } else {
        None
    }
}

/// What a touch or pen drag of `dx` px (negative is leftwards) asks for.
pub fn swipe_action(dx: f64) -> Option<KeyAction> {
    if !dx.is_finite() || dx.abs() < SWIPE_PX {
        None
    } else if dx < 0.0 {
        Some(KeyAction::Next)
    } else {
        Some(KeyAction::Prev)
    }
}

/// The locator to save for the page at `index` (from 0).
pub fn pdf_locator(index: usize) -> String {
    PageLocator::from_index(index).to_string()
}

/// The page index a saved locator names, held inside a book of `count`
/// pages; anything that is not a `pdf1` locator opens the first page.
pub fn pdf_saved_index(text: &str, count: usize) -> usize {
    text.parse::<PageLocator>()
        .map_or(0, PageLocator::index)
        .min(count.saturating_sub(1))
}

/// Progress through a PDF at the page `index`: the page over the count.
pub fn pdf_progress(index: usize, count: usize) -> f64 {
    page_progress(index, count)
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

    fn pending(locator: &str, read_at_ms: i64) -> PendingSave {
        PendingSave {
            locator: locator.into(),
            fraction: 0.5,
            read_at_ms,
        }
    }

    #[test]
    fn a_queued_save_replaces_an_older_one_but_not_a_newer() {
        let book = ItemRef::Real(7);
        let mut q = PendingSaves::default();
        q.queue(book, pending("a", 100));
        q.queue(book, pending("b", 200));
        q.queue(book, pending("stale", 150));
        assert_eq!(q.entries(), vec![(book, pending("b", 200))]);
        q.queue(ItemRef::Synth(2), pending("other", 50));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn a_queue_forgets_only_the_save_the_server_answered() {
        let book = ItemRef::Real(7);
        let mut q = PendingSaves::default();
        q.queue(book, pending("a", 100));
        q.queue(book, pending("b", 200));
        assert!(
            !q.answered(book, &pending("a", 100)),
            "a newer one is queued"
        );
        assert!(q.contains(book));
        assert!(q.answered(book, &pending("b", 200)));
        assert!(q.is_empty());
    }

    #[test]
    fn the_queue_round_trips_and_survives_garbage() {
        let mut q = PendingSaves::default();
        q.queue(ItemRef::Real(1), pending("epub:3:10", 1_700_000_000_123));
        q.queue(ItemRef::Real(2), pending("pdf:4", 1_700_000_000_456));
        assert_eq!(PendingSaves::parse(&q.to_text()), q);
        for text in ["", "nope", "[]", "{\"1\":"] {
            assert!(PendingSaves::parse(text).is_empty(), "{text}");
        }
        let (odd, dropped) = PendingSaves::parse_counting(
            r#"{"not an id":{"locator":"x","fraction":0.1,"read_at_ms":1},"7":{"locator":"y","fraction":0.2,"read_at_ms":2}}"#,
        );
        assert_eq!(
            (odd.len(), dropped),
            (1, 1),
            "a key that is no item id is dropped"
        );
        assert!(odd.contains(ItemRef::Real(7)));
    }

    #[test]
    fn entries_go_oldest_reading_first_and_the_queue_is_bounded() {
        let mut q = PendingSaves::default();
        for n in 0..=PENDING_MAX_ENTRIES as u64 {
            q.queue(ItemRef::Real(n), pending("x", 1_000 + n as i64));
        }
        assert_eq!(q.len(), PENDING_MAX_ENTRIES);
        assert!(!q.contains(ItemRef::Real(0)), "the oldest reading goes");
        let times: Vec<i64> = q.entries().iter().map(|(_, s)| s.read_at_ms).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn retries_back_off_to_a_ceiling() {
        let delays: Vec<u32> = (0..8).map(retry_delay_ms).collect();
        assert_eq!(
            delays,
            vec![2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000, 60_000]
        );
        assert_eq!(retry_delay_ms(500), RETRY_MAX_MS);
    }

    #[test]
    fn only_a_refusal_is_not_retried() {
        let api = |kind| ClientError::Api {
            kind,
            reason: String::new(),
            trace_id: String::new(),
        };
        // `ClientError::Rpc` (a dropped socket) is retried; capnp is not a
        // host dependency, so the Playwright offline test covers it.
        for kind in ["unavailable", "limit", "revoked"] {
            assert!(retryable(&api(kind)), "{kind} is held and retried");
            assert!(retryable_kind(kind), "{kind}");
        }
        for kind in ["invalid", "notFound", "notPermitted"] {
            assert!(!retryable(&api(kind)), "{kind}");
        }
    }

    #[test]
    fn settings_round_trip_through_their_text() {
        let s = ReaderSettings {
            font_px: 26,
            line_height_x10: 20,
            margin_px: 72,
            typeface: Typeface::Serif,
            theme: PageTheme::Night,
            pdf_fit: PdfFit::Width,
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
    fn line_height_reads_as_a_ratio_and_margins_have_names() {
        assert!((ReaderSettings::default().line_height() - 1.5).abs() < f64::EPSILON);
        let names: Vec<&str> = MARGINS.iter().map(|m| margin_label(*m)).collect();
        assert_eq!(names, ["None", "Narrow", "Medium", "Wide"]);
    }

    #[test]
    fn the_same_place_at_a_new_fraction_is_a_new_save() {
        assert_ne!(save_key("epub1:2:0", 0.5), save_key("epub1:2:0", 1.0));
        assert_eq!(save_key("epub1:2:0", 0.5), save_key("epub1:2:0", 0.5));
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

    fn press(key: &str) -> KeyPress<'_> {
        KeyPress {
            key,
            ctrl: false,
            meta: false,
            alt: false,
            shift: false,
            target: KeyTarget::Page,
        }
    }

    #[test]
    fn keys_turn_pages() {
        for k in ["ArrowRight", "PageDown", " "] {
            assert_eq!(key_action(press(k)), Some(KeyAction::Next), "{k}");
        }
        for k in ["ArrowLeft", "PageUp"] {
            assert_eq!(key_action(press(k)), Some(KeyAction::Prev), "{k}");
        }
        assert_eq!(key_action(press("Escape")), Some(KeyAction::Escape));
        assert_eq!(key_action(press("a")), None);
        assert_eq!(key_action(press("Enter")), None);
    }

    #[test]
    fn shift_space_turns_back() {
        let back = KeyPress {
            shift: true,
            ..press(" ")
        };
        assert_eq!(key_action(back), Some(KeyAction::Prev));
    }

    const TALL: HostScroll = HostScroll {
        top: 0.0,
        client_height: 700.0,
        scroll_height: 2000.0,
    };

    #[test]
    fn a_fit_page_pdf_only_turns() {
        let page = |k| pdf_key(press(k), PdfFit::Page, TALL);
        assert_eq!(page(" "), Some(PdfKey::Turn(true)));
        assert_eq!(page("PageUp"), Some(PdfKey::Turn(false)));
        assert_eq!(page("ArrowRight"), Some(PdfKey::Turn(true)));
        assert_eq!(page("ArrowDown"), None, "nothing to scroll");
        assert_eq!(page("Escape"), None);
        assert_eq!(page("a"), None);
    }

    #[test]
    fn a_fit_width_pdf_scrolls_to_its_end_before_it_turns() {
        let width = |k, scroll| pdf_key(press(k), PdfFit::Width, scroll);
        let at = |top| HostScroll { top, ..TALL };
        for k in [" ", "PageDown", "ArrowDown"] {
            assert_eq!(width(k, at(0.0)), Some(PdfKey::Scroll(630.0)), "{k}");
            assert_eq!(width(k, at(600.0)), Some(PdfKey::Scroll(630.0)), "{k}");
            assert_eq!(
                width(k, at(1300.0)),
                Some(PdfKey::Turn(true)),
                "{k} at the end"
            );
        }
        for k in ["PageUp", "ArrowUp"] {
            assert_eq!(width(k, at(1300.0)), Some(PdfKey::Scroll(-630.0)), "{k}");
            assert_eq!(
                width(k, at(0.0)),
                Some(PdfKey::Turn(false)),
                "{k} at the start"
            );
        }
        let back = KeyPress {
            shift: true,
            ..press(" ")
        };
        assert_eq!(
            pdf_key(back, PdfFit::Width, at(500.0)),
            Some(PdfKey::Scroll(-630.0))
        );
        // Left and right turn whatever the scroll.
        assert_eq!(width("ArrowRight", at(500.0)), Some(PdfKey::Turn(true)));
        assert_eq!(width("ArrowLeft", at(500.0)), Some(PdfKey::Turn(false)));
    }

    #[test]
    fn a_fit_width_page_that_fits_the_window_just_turns() {
        let short = HostScroll {
            top: 0.0,
            client_height: 700.0,
            scroll_height: 700.0,
        };
        assert_eq!(
            pdf_key(press("PageDown"), PdfFit::Width, short),
            Some(PdfKey::Turn(true))
        );
        assert_eq!(
            pdf_key(press("PageUp"), PdfFit::Width, short),
            Some(PdfKey::Turn(false))
        );
        let unknown = HostScroll {
            top: f64::NAN,
            ..TALL
        };
        assert_eq!(
            pdf_key(press("PageDown"), PdfFit::Width, unknown),
            Some(PdfKey::Turn(true))
        );
    }

    #[test]
    fn pdf_scrolling_keeps_the_modifier_and_focus_rules() {
        let chord = KeyPress {
            ctrl: true,
            ..press("ArrowDown")
        };
        assert_eq!(pdf_key(chord, PdfFit::Width, TALL), None);
        let field = KeyPress {
            target: KeyTarget::Field,
            ..press("ArrowDown")
        };
        assert_eq!(pdf_key(field, PdfFit::Width, TALL), None);
        let control = KeyPress {
            target: KeyTarget::Control,
            ..press(" ")
        };
        assert_eq!(
            pdf_key(control, PdfFit::Width, TALL),
            None,
            "a button keeps Space"
        );
    }

    #[test]
    fn the_browsers_own_chords_are_left_alone() {
        for (ctrl, meta, alt) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let chord = KeyPress {
                ctrl,
                meta,
                alt,
                ..press("ArrowRight")
            };
            assert_eq!(key_action(chord), None, "{ctrl} {meta} {alt}");
        }
    }

    #[test]
    fn a_focused_control_keeps_space_and_a_field_keeps_everything() {
        let on = |key, target| KeyPress {
            target,
            ..press(key)
        };
        assert_eq!(key_action(on(" ", KeyTarget::Control)), None);
        assert_eq!(
            key_action(on("ArrowRight", KeyTarget::Control)),
            Some(KeyAction::Next)
        );
        for k in [" ", "ArrowRight", "Escape"] {
            assert_eq!(key_action(on(k, KeyTarget::Field)), None, "{k}");
        }
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
    fn a_pdf_says_its_page_and_an_epub_only_its_percent() {
        assert_eq!(progress_label(0.25, Some((2, 12))), "Page 3 of 12 · 25%");
        assert_eq!(progress_label(0.25, None), "25%");
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
    fn the_title_page_offers_what_the_format_allows() {
        let book = |format| Book {
            format,
            author: String::new(),
            page_count: 0,
        };
        let at = |fraction| ReadPosition {
            locator: "epub1:1:0".into(),
            fraction,
        };
        assert_eq!(
            book_action(&book(BookFormat::Epub), None),
            BookAction::Read("Read".into())
        );
        assert_eq!(
            book_action(&book(BookFormat::Epub), Some(&at(0.0))),
            BookAction::Read("Read".into())
        );
        assert_eq!(
            book_action(&book(BookFormat::Epub), Some(&at(0.5))),
            BookAction::Read("Continue reading · 50%".into())
        );
        assert_eq!(
            book_action(&book(BookFormat::Pdf), Some(&at(0.5))),
            BookAction::Read("Continue reading · 50%".into())
        );
        assert_eq!(
            book_action(&book(BookFormat::Pdf), None),
            BookAction::Read("Read".into())
        );
        for format in [BookFormat::Unreadable, BookFormat::Comic] {
            assert!(
                matches!(
                    book_action(&book(format), Some(&at(0.5))),
                    BookAction::Closed(_)
                ),
                "{format:?}"
            );
        }
        assert!(matches!(
            book_action(&book(BookFormat::Unreadable), None),
            BookAction::Closed(line) if line.contains("Kindle")
        ));
        assert_eq!(download_href(ItemRef::Real(9)), "/Items/9/Download");
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
    fn a_page_fits_the_window_or_its_width() {
        // A portrait page (aspect 0.5) in a 1280 x 800 window at ratio 1.
        let page = pdf_layout(1280.0, 800.0, 0.5, PdfFit::Page, 1.0);
        assert!((page.css_height - 800.0).abs() < 1e-9);
        assert!((page.css_width - 400.0).abs() < 1e-9);
        assert_eq!(page.render_height, 800);
        let width = pdf_layout(1280.0, 800.0, 0.5, PdfFit::Width, 1.0);
        assert!((width.css_width - 1280.0).abs() < 1e-9);
        assert!(
            (width.css_height - 2560.0).abs() < 1e-9,
            "taller than the window"
        );
        // A landscape page in a narrow window is limited by width.
        let wide = pdf_layout(400.0, 800.0, 2.0, PdfFit::Page, 1.0);
        assert!((wide.css_width - 400.0).abs() < 1e-9);
        assert!((wide.css_height - 200.0).abs() < 1e-9);
    }

    #[test]
    fn the_backing_store_follows_the_pixel_ratio_up_to_the_caps() {
        let at = |dpr| pdf_layout(1280.0, 800.0, 0.5, PdfFit::Page, dpr).render_height;
        assert_eq!(at(1.0), 800);
        assert_eq!(at(2.0), 1600);
        assert_eq!(
            at(3.0),
            1600,
            "a ratio of 3 is held to {PDF_MAX_PIXEL_RATIO}"
        );
        // Fit width on a tall page would need 5120 px; the side cap holds it.
        let tall = pdf_layout(1280.0, 800.0, 0.5, PdfFit::Width, 2.0);
        assert_eq!(tall.render_height, PDF_MAX_SIDE_PX);
        // A very wide page is drawn shorter so its width stays under the cap.
        let wide = pdf_layout(3000.0, 3000.0, 4.0, PdfFit::Width, 2.0);
        assert_eq!(wide.render_height, PDF_MAX_SIDE_PX / 4);
    }

    #[test]
    fn a_window_or_page_without_a_size_still_gets_a_pixel() {
        for (w, h, a, d) in [
            (0.0, 0.0, 0.0, 0.0),
            (f64::NAN, 100.0, f64::INFINITY, f64::NAN),
            (-5.0, -5.0, -1.0, -1.0),
        ] {
            let l = pdf_layout(w, h, a, PdfFit::Page, d);
            assert!(l.render_height >= 1, "{l:?}");
            assert!(l.css_width.is_finite() && l.css_height.is_finite(), "{l:?}");
        }
    }

    #[test]
    fn page_turns_stop_at_the_ends_and_only_the_next_page_is_drawn_ahead() {
        assert_eq!(turn_to(0, 3, true), Some(1));
        assert_eq!(turn_to(2, 3, true), None);
        assert_eq!(turn_to(0, 3, false), None);
        assert_eq!(turn_to(2, 3, false), Some(1));
        assert_eq!(turn_to(0, 0, true), None);
        assert_eq!(prerender_index(0, 3), Some(1));
        assert_eq!(prerender_index(2, 3), None, "nothing past the last page");
    }

    #[test]
    fn the_outer_thirds_turn_and_a_swipe_needs_forty_pixels() {
        assert_eq!(tap_action(0.1), Some(KeyAction::Prev));
        assert_eq!(tap_action(0.5), None);
        assert_eq!(tap_action(0.9), Some(KeyAction::Next));
        assert_eq!(swipe_action(-41.0), Some(KeyAction::Next));
        assert_eq!(swipe_action(41.0), Some(KeyAction::Prev));
        assert_eq!(swipe_action(-39.0), None);
        assert_eq!(swipe_action(f64::NAN), None);
    }

    #[test]
    fn a_pdf_position_is_its_page_and_restores_inside_the_book() {
        assert_eq!(pdf_locator(0), "pdf1:1");
        assert_eq!(pdf_locator(11), "pdf1:12");
        assert_eq!(pdf_saved_index("pdf1:12", 30), 11);
        assert_eq!(
            pdf_saved_index("pdf1:12", 5),
            4,
            "a shorter file holds at its end"
        );
        assert_eq!(pdf_saved_index("epub1:3:0", 30), 0);
        assert_eq!(pdf_saved_index("", 30), 0);
        assert_eq!(pdf_saved_index("pdf1:3", 0), 0);
        assert!((pdf_progress(1, 4) - 0.5).abs() < 1e-9);
        assert!((pdf_progress(3, 4) - 1.0).abs() < 1e-9);
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
