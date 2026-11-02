//! What the watch page decides without touching the browser: which codecs to
//! offer the server, which engine plays the stream, and when to report
//! progress. The browser answers come in as plain values, so all of it is
//! tested on the host.

use pharos_client::api::Capabilities;

/// Something that turns a stream URL into playback on the page's `<video>`
/// (009 Boundaries: each external dependency sits behind an adapter). The
/// engine is dropped when the player unmounts; dropping detaches it.
pub trait PlaybackEngine {
    /// Start `url` on the `<video>` with id `video_id`, `start_ms` in.
    fn attach(&mut self, video_id: &str, url: &str, start_ms: u64) -> Result<(), String>;
}

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

/// A `<video>` event the report clock cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEvent {
    Playing,
    Pause,
    Seeked,
    TimeUpdate,
    Ended,
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
        };
        self.last_report_ms = Some(now_ms);
        Some(send)
    }

    /// Whether a stop is still owed: true once, then false.
    pub fn stop(&mut self) -> bool {
        !std::mem::replace(&mut self.stopped, true)
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn play_starts_at_the_stored_position_unless_finished() {
        assert_eq!(start_ms(90_000, false), 90_000);
        assert_eq!(start_ms(90_000, true), 0);
        assert_eq!(start_ms(0, false), 0);
    }
}
