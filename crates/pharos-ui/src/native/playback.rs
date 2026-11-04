//! What the watch page decides without touching the browser: which codecs to
//! offer the server, which engine plays the stream, and when to report
//! progress. The browser answers come in as plain values, so all of it is
//! tested on the host.

use pharos_client::api::{Capabilities, Segment, SegmentKind, Track, Trickplay};

/// Something that turns a stream URL into playback on the page's `<video>`
/// (009 Boundaries: each external dependency sits behind an adapter). The
/// engine is dropped when the player unmounts; dropping detaches it.
pub trait PlaybackEngine {
    /// Start `url` on the `<video>` with id `video_id`, `start_ms` in.
    /// `on_fatal` gets the reason once playback has failed for good.
    fn attach(
        &mut self,
        video_id: &str,
        url: &str,
        start_ms: u64,
        on_fatal: OnFatal,
    ) -> Result<(), String>;
}

/// Told why playback stopped for good.
pub type OnFatal = std::rc::Rc<dyn Fn(String)>;

/// Which engine plays a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// A file the `<video>` element plays from its URL.
    Direct,
    /// An HLS playlist the `<video>` element plays itself (Safari).
    NativeHls,
    /// An HLS playlist played through Media Source Extensions by hls.js.
    HlsJs,
    /// HLS with neither native support nor MSE.
    Unsupported,
}

/// The engine for a stream: native HLS where the browser has it, else hls.js
/// where Media Source Extensions exist.
pub fn choose_engine(is_hls: bool, native_hls: bool, mse: bool) -> Engine {
    match (is_hls, native_hls, mse) {
        (false, _, _) => Engine::Direct,
        (true, true, _) => Engine::NativeHls,
        (true, false, true) => Engine::HlsJs,
        (true, false, false) => Engine::Unsupported,
    }
}

/// Server codec name → the MIME type a browser is asked about. Codecs are
/// probed in fragmented MP4, the packaging pharos's HLS segments use, so the
/// answer holds for direct play and HLS alike.
const VIDEO_CODECS: &[(&str, &str)] = &[
    ("h264", r#"video/mp4; codecs="avc1.640028""#),
    ("hevc", r#"video/mp4; codecs="hvc1.1.6.L120.90""#),
    ("vp9", r#"video/mp4; codecs="vp09.00.40.08""#),
    ("av1", r#"video/mp4; codecs="av01.0.08M.08""#),
];

const AUDIO_CODECS: &[(&str, &str)] = &[
    ("aac", r#"audio/mp4; codecs="mp4a.40.2""#),
    ("opus", r#"audio/mp4; codecs="opus""#),
    ("flac", r#"audio/mp4; codecs="flac""#),
    ("mp3", "audio/mpeg"),
    ("ac3", r#"audio/mp4; codecs="ac-3""#),
    ("eac3", r#"audio/mp4; codecs="ec-3""#),
];

/// Containers a `<video>` element opens directly.
const CONTAINERS: &[(&str, &str)] = &[
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    ("mkv", "video/x-matroska"),
];

/// The capabilities to negotiate with, from the browser's answers: `codec`
/// for a codec MIME type, `container` for a container one.
pub fn capabilities(
    codec: impl Fn(&str) -> bool,
    container: impl Fn(&str) -> bool,
) -> Capabilities {
    let pick = |table: &[(&str, &str)], ok: &dyn Fn(&str) -> bool| -> Vec<String> {
        table
            .iter()
            .filter(|(_, mime)| ok(mime))
            .map(|(name, _)| (*name).to_owned())
            .collect()
    };
    Capabilities {
        video_codecs: pick(VIDEO_CODECS, &codec),
        audio_codecs: pick(AUDIO_CODECS, &codec),
        containers: pick(CONTAINERS, &container),
        max_bitrate: 0,
    }
}

/// The `<video>` as the controls show it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaState {
    pub position_ms: u64,
    /// 0 until the metadata has loaded.
    pub duration_ms: u64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
}

impl Default for MediaState {
    fn default() -> Self {
        Self {
            position_ms: 0,
            duration_ms: 0,
            paused: true,
            volume: 1.0,
            muted: false,
        }
    }
}

/// A `<video>` event the report clock cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEvent {
    Playing,
    Pause,
    Seeked,
    TimeUpdate,
    Ended,
    /// Volume, duration or metadata changed: the controls redraw, nothing
    /// is reported.
    Changed,
}

/// What to send the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outgoing {
    Report { paused: bool },
    Stop,
}

/// Progress reports while playing. The server writes a report at once when
/// the pause state flips or the position jumps, and coalesces the rest, so
/// this matches its ~10 s cadence.
pub const REPORT_INTERVAL_MS: f64 = 10_000.0;

/// When a playback reports: on play, pause and seek at once, every
/// [`REPORT_INTERVAL_MS`] while playing, and a single stop at the end
/// (ended, unmount or page hide, whichever comes first).
#[derive(Debug, Clone, Default)]
pub struct ReportClock {
    last_report_ms: Option<f64>,
    paused: bool,
    stopped: bool,
}

impl ReportClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// The send `event` at `now_ms` (a monotonic clock) calls for.
    pub fn on(&mut self, now_ms: f64, event: MediaEvent) -> Option<Outgoing> {
        if self.stopped {
            return None;
        }
        let send = match event {
            MediaEvent::Playing => {
                self.paused = false;
                Outgoing::Report { paused: false }
            }
            MediaEvent::Pause => {
                self.paused = true;
                Outgoing::Report { paused: true }
            }
            MediaEvent::Seeked => Outgoing::Report {
                paused: self.paused,
            },
            MediaEvent::TimeUpdate => {
                let due = self
                    .last_report_ms
                    .map_or(true, |last| now_ms - last >= REPORT_INTERVAL_MS);
                if self.paused || !due {
                    return None;
                }
                Outgoing::Report { paused: false }
            }
            MediaEvent::Ended => return self.stop().then_some(Outgoing::Stop),
            MediaEvent::Changed => return None,
        };
        self.last_report_ms = Some(now_ms);
        Some(send)
    }

    /// Whether a stop is still owed: true once, then false.
    pub fn stop(&mut self) -> bool {
        !std::mem::replace(&mut self.stopped, true)
    }
}

/// What a fatal hls.js error calls for (hls.js's own recovery calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// `startLoad()`: reload from where loading stopped.
    StartLoad,
    /// `recoverMediaError()`: rebuild the media buffers.
    RecoverMedia,
    /// Give up and show the error.
    Fail,
}

impl Recovery {
    /// The label the error log carries.
    pub fn label(self) -> &'static str {
        match self {
            Recovery::StartLoad => "start_load",
            Recovery::RecoverMedia => "recover_media",
            Recovery::Fail => "fail",
        }
    }
}

/// Recoveries of one kind before a fatal error is shown. A fragment that
/// loads refills the budget, so a long film survives scattered blips but a
/// stream that keeps failing gives up within a few seconds.
pub const MAX_RECOVERIES: u32 = 3;

/// Chooses the recovery for each fatal hls.js error.
#[derive(Debug, Clone, Default)]
pub struct RecoveryBudget {
    network: u32,
    media: u32,
}

impl RecoveryBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// The recovery for a fatal error of hls.js `kind` (`ErrorTypes`) and
    /// `details` (`ErrorDetails`).
    pub fn on_fatal(&mut self, kind: &str, details: &str) -> Recovery {
        let (used, recovery) = match kind {
            // Nothing loaded yet to restart from: `startLoad` would do nothing.
            "networkError" if details.starts_with("manifest") => return Recovery::Fail,
            "networkError" => (&mut self.network, Recovery::StartLoad),
            "mediaError" => (&mut self.media, Recovery::RecoverMedia),
            _ => return Recovery::Fail,
        };
        if *used >= MAX_RECOVERIES {
            return Recovery::Fail;
        }
        *used += 1;
        recovery
    }

    /// The attempts used so far, for the error log.
    pub fn attempts(&self, kind: &str) -> u32 {
        match kind {
            "networkError" => self.network,
            "mediaError" => self.media,
            _ => 0,
        }
    }

    /// A fragment loaded: playback is moving again.
    pub fn progressed(&mut self) {
        *self = Self::default();
    }
}

/// Where Play starts: the stored position of a started, unfinished item.
pub fn start_ms(position_ms: u64, played: bool) -> u64 {
    if played {
        0
    } else {
        position_ms
    }
}

/// One trickplay thumbnail: the tile it is on and where in the tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumb {
    pub url: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The thumbnail nearest before `at_ms`, clamped to the last one; `None`
/// for a set with no thumbnails.
pub fn thumb_at(set: &Trickplay, at_ms: u64) -> Option<Thumb> {
    let per_tile = set.grid.checked_mul(set.grid).filter(|&n| n > 0)?;
    let last = set.thumb_count.checked_sub(1)?;
    let n = (at_ms / u64::from(set.interval_ms.max(1))).min(u64::from(last));
    let n = u32::try_from(n).unwrap_or(last);
    let (tile, within) = (n / per_tile, n % per_tile);
    Some(Thumb {
        url: set.tile_url.replace("{tile}", &tile.to_string()),
        x: (within % set.grid).saturating_mul(set.width),
        y: (within / set.grid).saturating_mul(set.height),
        width: set.width,
        height: set.height,
    })
}

/// The narrowest sprite set: the seek-bar preview is small, and the
/// narrowest tiles load fastest.
pub fn preview_set(sets: &[Trickplay]) -> Option<&Trickplay> {
    sets.iter().min_by_key(|s| s.width)
}

/// The segment playing at `at_ms`, whose skip button shows.
pub fn segment_at(segments: &[Segment], at_ms: u64) -> Option<Segment> {
    segments
        .iter()
        .find(|s| s.start_ms <= at_ms && at_ms < s.end_ms)
        .copied()
}

pub fn skip_label(kind: SegmentKind) -> &'static str {
    match kind {
        SegmentKind::Intro => "Skip intro",
        SegmentKind::Outro => "Skip credits",
        SegmentKind::Recap => "Skip recap",
        SegmentKind::Preview => "Skip preview",
        SegmentKind::Commercial => "Skip ad",
    }
}

/// `at_ms` as a percentage of `total_ms`, for a left offset on the seek bar.
pub fn percent(at_ms: u64, total_ms: u64) -> f64 {
    if total_ms == 0 {
        return 0.0;
    }
    // Lossy for positions past 2^53 ms, far beyond any runtime.
    (at_ms.min(total_ms) as f64 / total_ms as f64) * 100.0
}

/// The position a fraction of the way along a seek bar `width` wide. `None`
/// while the length is unknown, as for a live stream, so a click seeks nowhere
/// rather than to the start.
pub fn position_at(x: f64, width: f64, total_ms: u64) -> Option<u64> {
    if total_ms == 0 || width.is_nan() || width <= 0.0 || !x.is_finite() {
        return None;
    }
    let fraction = (x / width).clamp(0.0, 1.0);
    // Saturating cast of a value already within 0..=total_ms.
    Some((fraction * total_ms as f64) as u64)
}

/// "4:05", "1:02:03".
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// How a track is named in a picker: its title, else its language, else its
/// position, with the forced and SDH flags the title does not already say.
pub fn track_label(track: &Track, position: usize) -> String {
    let mut label = [&track.title, &track.language]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("Track {}", position.saturating_add(1)));
    let lower = label.to_lowercase();
    if track.is_forced && !lower.contains("forced") {
        label.push_str(" (forced)");
    }
    if track.is_hearing_impaired && !lower.contains("sdh") {
        label.push_str(" [SDH]");
    }
    label
}

/// How the player moves from the subtitle it shows to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleSwitch {
    /// Show or hide WebVTT on the `<video>`; the stream plays on.
    InPlace,
    /// The server must start or stop burning a subtitle: a new playback.
    Renegotiate,
}

/// `burned` is whether the playing stream has a subtitle drawn in; `to` the
/// track picked, `None` for off.
pub fn subtitle_switch(burned: bool, to: Option<&Track>) -> SubtitleSwitch {
    match (burned, to) {
        (true, _) | (false, Some(Track { vtt_url: None, .. })) => SubtitleSwitch::Renegotiate,
        (false, _) => SubtitleSwitch::InPlace,
    }
}

/// The original script of an ASS/SSA text subtitle, for a renderer that
/// draws its typesetting (ADR-0046); `None` for any other track, which plays
/// as WebVTT. The server serves the script beside the WebVTT it converts it
/// to, at `stream.ass` in place of `stream.vtt`.
pub fn styled_subtitle_url(track: &Track) -> Option<String> {
    if !matches!(track.codec.to_ascii_lowercase().as_str(), "ass" | "ssa") {
        return None;
    }
    let vtt = track.vtt_url.as_deref()?;
    vtt.strip_suffix("/stream.vtt")
        .map(|base| format!("{base}/stream.ass"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sprites() -> Trickplay {
        Trickplay {
            width: 320,
            height: 180,
            interval_ms: 10_000,
            thumb_count: 250,
            tile_count: 3,
            grid: 10,
            tile_url: "/videos/7/trickplay/320/{tile}.jpg".into(),
        }
    }

    #[test]
    fn a_thumbnail_is_found_by_tile_column_and_row() {
        let set = sprites();
        let at = |ms| thumb_at(&set, ms).map(|t| (t.url, t.x, t.y));
        assert_eq!(at(0), Some(("/videos/7/trickplay/320/0.jpg".into(), 0, 0)));
        // Thumbnail 23: tile 0, row 2, column 3.
        assert_eq!(
            at(235_000),
            Some(("/videos/7/trickplay/320/0.jpg".into(), 960, 360))
        );
        // Thumbnail 105: tile 1, row 0, column 5.
        assert_eq!(
            at(1_050_000),
            Some(("/videos/7/trickplay/320/1.jpg".into(), 1600, 0))
        );
        // Past the end: the last thumbnail, 249 (tile 2, row 4, column 9).
        assert_eq!(
            at(99_000_000),
            Some(("/videos/7/trickplay/320/2.jpg".into(), 2880, 720))
        );
        let empty = Trickplay {
            thumb_count: 0,
            ..sprites()
        };
        assert_eq!(thumb_at(&empty, 0), None);
    }

    #[test]
    fn the_preview_uses_the_narrowest_sprites() {
        let wide = Trickplay {
            width: 640,
            ..sprites()
        };
        let sets = [wide, sprites()];
        assert_eq!(preview_set(&sets).map(|s| s.width), Some(320));
        assert_eq!(preview_set(&[]), None);
    }

    #[test]
    fn the_skip_button_shows_only_inside_a_segment() {
        let intro = Segment {
            kind: SegmentKind::Intro,
            start_ms: 5_000,
            end_ms: 65_000,
        };
        let segments = [intro];
        assert_eq!(segment_at(&segments, 4_999), None);
        assert_eq!(segment_at(&segments, 5_000), Some(intro));
        assert_eq!(segment_at(&segments, 64_999), Some(intro));
        assert_eq!(segment_at(&segments, 65_000), None);
        assert_eq!(skip_label(intro.kind), "Skip intro");
    }

    #[test]
    fn seek_bar_maths_clamps_to_the_runtime() {
        assert_eq!(percent(30_000, 120_000), 25.0);
        assert_eq!(percent(500_000, 120_000), 100.0);
        assert_eq!(percent(1, 0), 0.0);
        assert_eq!(position_at(50.0, 200.0, 120_000), Some(30_000));
        assert_eq!(position_at(-5.0, 200.0, 120_000), Some(0));
        assert_eq!(position_at(400.0, 200.0, 120_000), Some(120_000));
        assert_eq!(position_at(10.0, 0.0, 120_000), None);
        assert_eq!(position_at(10.0, 200.0, 0), None);
    }

    #[test]
    fn clock_shows_hours_only_when_there_are_some() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(245_000), "4:05");
        assert_eq!(clock(3_723_000), "1:02:03");
    }

    #[test]
    fn tracks_are_named_by_title_then_language_then_position() {
        let mut t = Track {
            language: "eng".into(),
            ..Track::default()
        };
        assert_eq!(track_label(&t, 0), "eng");
        t.title = "Commentary".into();
        assert_eq!(track_label(&t, 0), "Commentary");
        t.is_forced = true;
        assert_eq!(track_label(&t, 0), "Commentary (forced)");
        assert_eq!(track_label(&Track::default(), 2), "Track 3");
    }

    #[test]
    fn only_a_burned_subtitle_needs_a_new_playback() {
        let text = Track {
            vtt_url: Some("/videos/7/subtitles/3/stream.vtt".into()),
            ..Track::default()
        };
        let image = Track::default();
        assert_eq!(subtitle_switch(false, Some(&text)), SubtitleSwitch::InPlace);
        assert_eq!(subtitle_switch(false, None), SubtitleSwitch::InPlace);
        assert_eq!(
            subtitle_switch(false, Some(&image)),
            SubtitleSwitch::Renegotiate
        );
        assert_eq!(
            subtitle_switch(true, Some(&text)),
            SubtitleSwitch::Renegotiate
        );
        assert_eq!(subtitle_switch(true, None), SubtitleSwitch::Renegotiate);
    }

    #[test]
    fn only_an_ass_text_subtitle_is_drawn_from_its_script() {
        let track = |codec: &str, vtt: Option<&str>| Track {
            codec: codec.into(),
            vtt_url: vtt.map(Into::into),
            ..Track::default()
        };
        let vtt = Some("/videos/7/subtitles/3/stream.vtt");
        assert_eq!(
            styled_subtitle_url(&track("ass", vtt)).as_deref(),
            Some("/videos/7/subtitles/3/stream.ass")
        );
        assert_eq!(
            styled_subtitle_url(&track("SSA", vtt)).as_deref(),
            Some("/videos/7/subtitles/3/stream.ass")
        );
        assert_eq!(styled_subtitle_url(&track("subrip", vtt)), None);
        assert_eq!(styled_subtitle_url(&track("webvtt", vtt)), None);
        // Burned, or a URL of another shape: nothing to fetch the script from.
        assert_eq!(styled_subtitle_url(&track("ass", None)), None);
        assert_eq!(
            styled_subtitle_url(&track("ass", Some("/elsewhere.vtt"))),
            None
        );
    }

    #[test]
    fn hls_prefers_native_then_hlsjs() {
        assert_eq!(choose_engine(false, false, false), Engine::Direct);
        assert_eq!(choose_engine(false, true, true), Engine::Direct);
        assert_eq!(choose_engine(true, true, true), Engine::NativeHls);
        assert_eq!(choose_engine(true, false, true), Engine::HlsJs);
        assert_eq!(choose_engine(true, false, false), Engine::Unsupported);
    }

    /// A browser without WebM still offers VP9: codecs are asked about in
    /// MP4, containers separately.
    #[test]
    fn capabilities_list_what_the_browser_said_yes_to() {
        let caps = capabilities(
            |mime| mime.contains("vp09") || mime.contains("opus") || mime == "audio/mpeg",
            |mime| mime == "video/mp4",
        );
        assert_eq!(caps.video_codecs, ["vp9"]);
        assert_eq!(caps.audio_codecs, ["opus", "mp3"]);
        assert_eq!(caps.containers, ["mp4", "m4v"]);
        assert_eq!(caps.max_bitrate, 0);
    }

    #[test]
    fn play_pause_and_seek_report_at_once() {
        let mut clock = ReportClock::new();
        assert_eq!(
            clock.on(0.0, MediaEvent::Playing),
            Some(Outgoing::Report { paused: false })
        );
        assert_eq!(
            clock.on(500.0, MediaEvent::Pause),
            Some(Outgoing::Report { paused: true })
        );
        assert_eq!(
            clock.on(600.0, MediaEvent::Seeked),
            Some(Outgoing::Report { paused: true })
        );
    }

    #[test]
    fn time_updates_report_every_interval_while_playing() {
        let mut clock = ReportClock::new();
        clock.on(0.0, MediaEvent::Playing);
        assert_eq!(clock.on(4_000.0, MediaEvent::TimeUpdate), None);
        assert_eq!(clock.on(9_999.0, MediaEvent::TimeUpdate), None);
        assert_eq!(
            clock.on(10_000.0, MediaEvent::TimeUpdate),
            Some(Outgoing::Report { paused: false })
        );
        assert_eq!(clock.on(15_000.0, MediaEvent::TimeUpdate), None);
        clock.on(16_000.0, MediaEvent::Pause);
        assert_eq!(clock.on(40_000.0, MediaEvent::TimeUpdate), None);
    }

    /// A time update before any play event still reports, so a stream that
    /// starts without `playing` firing is not silent.
    #[test]
    fn the_first_time_update_reports() {
        let mut clock = ReportClock::new();
        assert_eq!(
            clock.on(100.0, MediaEvent::TimeUpdate),
            Some(Outgoing::Report { paused: false })
        );
    }

    #[test]
    fn stop_is_sent_once_and_silences_the_clock() {
        let mut clock = ReportClock::new();
        clock.on(0.0, MediaEvent::Playing);
        assert_eq!(clock.on(1.0, MediaEvent::Ended), Some(Outgoing::Stop));
        assert!(!clock.stop());
        assert_eq!(clock.on(2.0, MediaEvent::Playing), None);

        let mut clock = ReportClock::new();
        assert!(clock.stop());
        assert_eq!(clock.on(1.0, MediaEvent::Ended), None);
    }

    #[test]
    fn fatal_errors_recover_a_few_times_then_fail() {
        let mut budget = RecoveryBudget::new();
        for _ in 0..MAX_RECOVERIES {
            assert_eq!(
                budget.on_fatal("networkError", "fragLoadError"),
                Recovery::StartLoad
            );
        }
        assert_eq!(
            budget.on_fatal("networkError", "fragLoadError"),
            Recovery::Fail
        );
        assert_eq!(budget.attempts("networkError"), MAX_RECOVERIES);
        assert_eq!(
            budget.on_fatal("mediaError", "bufferStalledError"),
            Recovery::RecoverMedia,
            "each kind has its own budget"
        );
    }

    #[test]
    fn a_loaded_fragment_refills_the_budget() {
        let mut budget = RecoveryBudget::new();
        for _ in 0..MAX_RECOVERIES {
            budget.on_fatal("mediaError", "bufferAppendError");
        }
        budget.progressed();
        assert_eq!(
            budget.on_fatal("mediaError", "bufferAppendError"),
            Recovery::RecoverMedia
        );
    }

    #[test]
    fn a_failed_manifest_and_other_errors_fail_at_once() {
        let mut budget = RecoveryBudget::new();
        assert_eq!(
            budget.on_fatal("networkError", "manifestLoadError"),
            Recovery::Fail
        );
        assert_eq!(
            budget.on_fatal("keySystemError", "keySystemNoKeys"),
            Recovery::Fail
        );
        assert_eq!(
            budget.on_fatal("otherError", "internalException"),
            Recovery::Fail
        );
    }

    #[test]
    fn recovery_labels_are_distinct() {
        let labels =
            [Recovery::StartLoad, Recovery::RecoverMedia, Recovery::Fail].map(Recovery::label);
        assert_eq!(labels, ["start_load", "recover_media", "fail"]);
    }

    #[test]
    fn play_starts_at_the_stored_position_unless_finished() {
        assert_eq!(start_ms(90_000, false), 90_000);
        assert_eq!(start_ms(90_000, true), 0);
        assert_eq!(start_ms(0, false), 0);
    }
}
