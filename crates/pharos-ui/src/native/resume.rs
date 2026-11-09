//! How long a paused song takes to sound again once the listener resumes it
//! (B563): from the resume to the element's `playing` event, then to the
//! first analyser frame with sound in it.

use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel};

/// A resume that has not sounded by now is reported as a timeout (V473).
pub const RESUME_TIMEOUT_MS: f64 = 5_000.0;

/// The time-domain peak, divided by the element's volume, above which a
/// frame counts as sound: -60 dBFS, below any music's quiet passages but
/// above the routing's own zero.
pub const AUDIBLE_PEAK: f32 = 0.001;

/// How long a paused song keeps its tap's `AudioContext` running (B734,
/// V683). A running context holds the browser's output stream open, so the
/// sound server does not suspend an idle output device under the pause and
/// the resume does not wait for the device to wake. After this the context
/// is suspended, sparing the device on a long pause.
pub const REST_AFTER_PAUSE_MS: f64 = 300_000.0;

/// Whether a song paused at `paused_at_ms` and not played since rests its
/// output by `now_ms`.
pub fn rests(paused_at_ms: f64, now_ms: f64) -> bool {
    now_ms - paused_at_ms >= REST_AFTER_PAUSE_MS
}

/// Whether a media element playing `src` on a page at `origin` can be routed
/// through Web Audio (V682): a cross-origin source without CORS would play
/// silence once routed, and WebKit's native HLS feeds a
/// `MediaElementAudioSourceNode` silence, so both are left unrouted. A
/// source fetched with CORS (`cors`: the TV app's songs, B1032) is readable
/// from another origin. The error names the refused source's path only: the
/// query can carry a media token.
pub fn routable(src: &str, origin: &str, cors: bool) -> Result<(), String> {
    let same_origin = src.starts_with("blob:")
        || (!origin.is_empty()
            && src
                .strip_prefix(origin)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/')));
    let path = src.split('?').next().unwrap_or_default();
    if !same_origin && !cors {
        return Err(format!("cross-origin source {path}"));
    }
    if path.ends_with(".m3u8") {
        return Err(format!("native HLS source {path}"));
    }
    Ok(())
}

/// What the resume played (B735).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Music,
    Video,
}

impl Item {
    pub fn label(self) -> &'static str {
        match self {
            Self::Music => "music",
            Self::Video => "video",
        }
    }
}

/// How the play's wake of a suspended tap context went (B735): `resume()`
/// settles after the play, so this is read when the report is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wake {
    /// The context was running, or there is no tap: nothing to wake.
    #[default]
    None,
    /// `resume()` was asked for and has not settled.
    Pending,
    /// `resume()` settled and the context runs.
    Resumed,
    /// `resume()`'s promise was rejected.
    Rejected,
    /// `resume()` threw before returning a promise.
    Refused,
}

impl Wake {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Resumed => "resumed",
            Self::Rejected => "rejected",
            Self::Refused => "refused",
        }
    }
}

/// Whether a play wakes a tap's `AudioContext` in `state` first: only
/// a suspended one, or one WebKit interrupted (a call or another app took
/// the audio session); a closed one cannot be resumed.
pub fn wakes_context(state: &str) -> bool {
    matches!(state, "suspended" | "interrupted")
}

/// How long a new tap's `AudioContext` may take to start before it is
/// reported stuck (B1141, V1101). Firefox starts one asynchronously, within
/// ~200 ms on a working output; with no sound output it never starts and
/// its `resume()` never settles (B1140).
pub const TAP_START_REPORT_MS: u32 = 3_000;

/// How a new tap's `AudioContext` started (B1141). Chromium makes one
/// running; Firefox makes one suspended and starts it a moment later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapStart {
    /// `resume()` settled and the context is in `state` (running, unless the
    /// browser said otherwise), `start_ms` after the tap was made; 0 when it
    /// was running from the start.
    Started { state: String, start_ms: u64 },
    /// Still in `state` with `resume()` unsettled after
    /// [`TAP_START_REPORT_MS`]: the browser has no output to open.
    Stuck { state: String },
    /// `resume()`'s promise was rejected with `error`, leaving `state`.
    Rejected { error: String, state: String },
    /// `resume()` threw `error` before returning a promise.
    Refused { error: String },
}

impl TapStart {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Stuck { .. } => "stuck",
            Self::Rejected { .. } => "rejected",
            Self::Refused { .. } => "refused",
        }
    }

    /// Whether the outcome is a fault, logged as a warning.
    pub fn is_fault(&self) -> bool {
        !matches!(self, Self::Started { state, .. } if state == "running")
    }

    /// The console line, naming the outcome and the values behind it.
    pub fn line(&self) -> String {
        let label = self.label();
        match self {
            Self::Started { state, start_ms } => {
                format!("pharos: visualiser tap context={label} state={state} start_ms={start_ms}")
            }
            Self::Stuck { state } => format!(
                "pharos: visualiser tap context={label} state={state} waited_ms={TAP_START_REPORT_MS}"
            ),
            Self::Rejected { error, state } => {
                format!("pharos: visualiser tap context={label} state={state} error={error}")
            }
            Self::Refused { error } => format!("pharos: visualiser tap context={label} error={error}"),
        }
    }
}

/// The Web Audio path the song is routed through, when it is tapped.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioPath {
    /// `AudioContext.state` at the resume: running, suspended or closed.
    pub context_state: String,
    pub base_latency_ms: Option<f64>,
    pub output_latency_ms: Option<f64>,
}

/// Attribute an MSE engine sets on the `<video>` it attaches to, naming itself:
/// both play from a `blob:` URL, so the source alone cannot tell them apart.
pub const ENGINE_MARK: &str = "data-pharos-engine";

/// The engine playing a `blob:` source, from the element's [`ENGINE_MARK`]
/// (B732). hls.js when unmarked, as before the Rust engine existed.
pub fn blob_engine(mark: Option<&str>) -> &'static str {
    match mark {
        Some("rust") => "rust",
        _ => "hlsjs",
    }
}

/// The media element as the resume found it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeContext {
    /// `HTMLMediaElement.readyState`, 0 to 4.
    pub ready_state: u16,
    /// Buffered media ahead of the playhead.
    pub buffered_ahead_ms: u64,
    /// `direct`, `hlsjs` or `rust` (MSE), or `native_hls`.
    pub engine: &'static str,
    pub muted: bool,
    /// `None` when the element is not routed through an analyser.
    pub audio: Option<AudioPath>,
    /// `navigator.userAgent` (B735).
    pub user_agent: String,
}

impl ResumeContext {
    /// No media element to read.
    pub fn unavailable() -> Self {
        Self {
            ready_state: 0,
            buffered_ahead_ms: 0,
            engine: "none",
            muted: false,
            audio: None,
            user_agent: String::new(),
        }
    }
}

/// How the sound after `playing` was, or was not, seen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Audible {
    /// Milliseconds from `playing` to the first frame with sound.
    Ms(u64),
    /// No analyser on the element, so sound cannot be seen.
    Untapped,
    /// Muted or at volume 0: there is no sound to see.
    Muted,
    /// Paused again before it sounded.
    Paused,
    /// Played again before it sounded, as after a refused `play()`.
    Superseded,
    Timeout,
}

impl Audible {
    fn label(self) -> String {
        match self {
            Self::Ms(ms) => ms.to_string(),
            Self::Untapped => "untapped".to_owned(),
            Self::Muted => "muted".to_owned(),
            Self::Paused => "paused".to_owned(),
            Self::Superseded => "superseded".to_owned(),
            Self::Timeout => "timeout".to_owned(),
        }
    }
}

/// One resume, measured.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeReport {
    /// `None` when `playing` did not come before the report.
    pub click_to_playing_ms: Option<u64>,
    pub playing_to_audible: Audible,
    pub context: ResumeContext,
    pub item: Item,
    /// How long the element was paused before the play; `None` when the
    /// pause was not seen.
    pub paused_ms: Option<u64>,
    /// How the play's wake of the tap context went, when the report is sent.
    pub wake: Wake,
}

impl ResumeReport {
    /// Ended by the clock, not by the listener.
    fn timed_out(&self) -> bool {
        match self.playing_to_audible {
            Audible::Timeout => true,
            Audible::Paused | Audible::Superseded => false,
            Audible::Ms(_) | Audible::Untapped | Audible::Muted => {
                self.click_to_playing_ms.is_none()
            }
        }
    }

    fn fields(&self) -> Vec<(&'static str, String)> {
        let ms = |v: Option<f64>| v.map_or_else(|| "none".to_owned(), |v| format!("{v:.1}"));
        let audio = self.context.audio.as_ref();
        vec![
            (
                "click_to_playing_ms",
                match self.click_to_playing_ms {
                    Some(ms) => ms.to_string(),
                    None if self.timed_out() => "timeout".to_owned(),
                    None => "none".to_owned(),
                },
            ),
            ("playing_to_audible_ms", self.playing_to_audible.label()),
            ("ready_state", self.context.ready_state.to_string()),
            (
                "buffered_ahead_ms",
                self.context.buffered_ahead_ms.to_string(),
            ),
            (
                "context_state",
                audio.map_or_else(|| "untapped".to_owned(), |a| a.context_state.clone()),
            ),
            ("base_latency_ms", ms(audio.and_then(|a| a.base_latency_ms))),
            (
                "output_latency_ms",
                ms(audio.and_then(|a| a.output_latency_ms)),
            ),
            (
                "context_resumed",
                audio
                    .is_some_and(|a| wakes_context(&a.context_state))
                    .to_string(),
            ),
            ("engine", self.context.engine.to_owned()),
            ("item", self.item.label().to_owned()),
            (
                "paused_ms",
                self.paused_ms
                    .map_or_else(|| "none".to_owned(), |ms| ms.to_string()),
            ),
            ("context_wake", self.wake.label().to_owned()),
            ("user_agent", self.context.user_agent.clone()),
        ]
    }

    /// The report with the wake's outcome as it stands now.
    pub fn with_wake(self, wake: Wake) -> Self {
        Self { wake, ..self }
    }

    /// The console line: `pharos: resume key=value ...`.
    pub fn line(&self) -> String {
        let pairs: Vec<String> = self
            .fields()
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!("pharos: resume {}", pairs.join(" "))
    }

    pub fn event(&self) -> ClientEvent {
        let slow = self.timed_out();
        ClientEvent {
            kind: ClientEventKind::PlaybackResume,
            level: if slow {
                ClientLevel::Warn
            } else {
                ClientLevel::Info
            },
            route: String::new(),
            fields: self.fields(),
        }
    }
}

/// One resume in flight. Each call that ends it returns the report, once.
#[derive(Debug, Clone)]
pub struct ResumeProbe {
    started_ms: f64,
    playing_ms: Option<f64>,
    /// A frame with sound seen before `playing` was.
    sounded_ms: Option<f64>,
    context: ResumeContext,
    item: Item,
    paused_ms: Option<u64>,
    done: bool,
}

fn whole_ms(from: f64, to: f64) -> u64 {
    // Never negative; lossless below 2^53 ms.
    (to - from).max(0.0).round() as u64
}

impl ResumeProbe {
    /// A play at `now_ms` of `item`, paused since `paused_at_ms` when that
    /// pause was seen.
    pub fn start(
        now_ms: f64,
        context: ResumeContext,
        item: Item,
        paused_at_ms: Option<f64>,
    ) -> Self {
        Self {
            started_ms: now_ms,
            playing_ms: None,
            sounded_ms: None,
            context,
            item,
            paused_ms: paused_at_ms.map(|at| whole_ms(at, now_ms)),
            done: false,
        }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    fn finish(&mut self, audible: Audible) -> Option<ResumeReport> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(ResumeReport {
            click_to_playing_ms: self.playing_ms.map(|p| whole_ms(self.started_ms, p)),
            playing_to_audible: audible,
            context: self.context.clone(),
            item: self.item,
            paused_ms: self.paused_ms,
            wake: Wake::None,
        })
    }

    /// The element's `playing` event.
    pub fn playing(&mut self, now_ms: f64) -> Option<ResumeReport> {
        if self.done || self.playing_ms.is_some() {
            return None;
        }
        self.playing_ms = Some(now_ms);
        if self.context.muted {
            return self.finish(Audible::Muted);
        }
        if self.context.audio.is_none() {
            return self.finish(Audible::Untapped);
        }
        match self.sounded_ms {
            Some(_) => self.finish(Audible::Ms(0)),
            None => None,
        }
    }

    /// A step of the clock, with the analyser's peak (already divided by the
    /// volume) when there is one to read.
    pub fn frame(&mut self, now_ms: f64, peak: Option<f32>) -> Option<ResumeReport> {
        if self.done {
            return None;
        }
        if peak.is_some_and(|p| p > AUDIBLE_PEAK) {
            match self.playing_ms {
                Some(playing) => return self.finish(Audible::Ms(whole_ms(playing, now_ms))),
                None => self.sounded_ms = self.sounded_ms.or(Some(now_ms)),
            }
        }
        if now_ms - self.started_ms < RESUME_TIMEOUT_MS {
            return None;
        }
        let audible = match (self.context.muted, &self.context.audio) {
            (true, _) => Audible::Muted,
            (false, None) => Audible::Untapped,
            (false, Some(_)) => Audible::Timeout,
        };
        self.finish(audible)
    }

    /// Paused again while the resume was in flight.
    pub fn paused(&mut self) -> Option<ResumeReport> {
        self.finish(Audible::Paused)
    }

    /// Played again while the resume was in flight.
    pub fn superseded(&mut self) -> Option<ResumeReport> {
        self.finish(Audible::Superseded)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn a_blob_source_names_the_engine_that_marked_it() {
        assert_eq!(super::blob_engine(Some("rust")), "rust");
        assert_eq!(super::blob_engine(Some("hlsjs")), "hlsjs");
        assert_eq!(super::blob_engine(None), "hlsjs");
    }

    fn tapped() -> ResumeContext {
        ResumeContext {
            ready_state: 4,
            buffered_ahead_ms: 180_000,
            engine: "direct",
            muted: false,
            audio: Some(AudioPath {
                context_state: "running".to_owned(),
                base_latency_ms: Some(10.0),
                output_latency_ms: None,
            }),
            user_agent: "Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0"
                .to_owned(),
        }
    }

    #[test]
    fn a_resume_is_timed_to_playing_and_then_to_sound() {
        let mut probe = ResumeProbe::start(1_000.0, tapped(), Item::Music, None);
        assert_eq!(probe.frame(1_016.0, Some(0.0)), None);
        assert_eq!(probe.playing(1_040.0), None);
        assert_eq!(probe.frame(1_056.0, Some(0.0005)), None, "below the floor");
        let report = probe.frame(1_290.0, Some(0.2)).expect("sound ends it");
        assert_eq!(report.click_to_playing_ms, Some(40));
        assert_eq!(report.playing_to_audible, Audible::Ms(250));
        assert!(probe.done());
        assert_eq!(probe.frame(1_300.0, Some(0.2)), None, "reported once");
    }

    #[test]
    fn sound_seen_before_playing_counts_as_zero() {
        let mut probe = ResumeProbe::start(0.0, tapped(), Item::Music, None);
        assert_eq!(probe.frame(10.0, Some(0.3)), None);
        let report = probe.playing(30.0).expect("playing ends it");
        assert_eq!(report.click_to_playing_ms, Some(30));
        assert_eq!(report.playing_to_audible, Audible::Ms(0));
    }

    #[test]
    fn no_sound_within_the_limit_is_a_timeout() {
        let mut probe = ResumeProbe::start(0.0, tapped(), Item::Music, None);
        probe.playing(100.0);
        assert_eq!(probe.frame(4_999.0, Some(0.0)), None);
        let report = probe.frame(5_000.0, Some(0.0)).expect("timed out");
        assert_eq!(report.click_to_playing_ms, Some(100));
        assert_eq!(report.playing_to_audible, Audible::Timeout);
        assert_eq!(report.event().level, ClientLevel::Warn);
    }

    #[test]
    fn no_playing_within_the_limit_is_a_timeout_too() {
        let mut probe = ResumeProbe::start(0.0, tapped(), Item::Music, None);
        let report = probe.frame(6_000.0, None).expect("timed out");
        assert_eq!(report.click_to_playing_ms, None);
        assert_eq!(report.playing_to_audible, Audible::Timeout);
        assert!(report.line().contains("click_to_playing_ms=timeout"));
    }

    #[test]
    fn an_untapped_or_muted_song_reports_at_playing() {
        let untapped = ResumeContext {
            audio: None,
            ..tapped()
        };
        let report = ResumeProbe::start(0.0, untapped, Item::Music, None)
            .playing(70.0)
            .expect("nothing to wait for");
        assert_eq!(report.playing_to_audible, Audible::Untapped);
        assert!(report.line().contains("context_state=untapped"));

        let muted = ResumeContext {
            muted: true,
            ..tapped()
        };
        let report = ResumeProbe::start(0.0, muted, Item::Music, None)
            .playing(70.0)
            .expect("muted");
        assert_eq!(report.playing_to_audible, Audible::Muted);
    }

    #[test]
    fn a_pause_before_sound_still_reports() {
        let mut probe = ResumeProbe::start(0.0, tapped(), Item::Music, None);
        probe.playing(50.0);
        let report = probe.paused().expect("paused ends it");
        assert_eq!(report.playing_to_audible, Audible::Paused);
        assert_eq!(probe.paused(), None);
    }

    #[test]
    fn a_pause_or_a_second_play_before_playing_is_not_a_timeout() {
        let report = ResumeProbe::start(0.0, tapped(), Item::Music, None)
            .paused()
            .expect("paused");
        assert!(
            report.line().contains("click_to_playing_ms=none"),
            "{}",
            report.line()
        );
        assert_eq!(report.event().level, ClientLevel::Info);

        let mut probe = ResumeProbe::start(0.0, tapped(), Item::Music, None);
        let report = probe.superseded().expect("superseded");
        assert_eq!(report.playing_to_audible, Audible::Superseded);
        assert!(report.line().contains("playing_to_audible_ms=superseded"));
        assert_eq!(report.event().level, ClientLevel::Info);
        assert_eq!(probe.superseded(), None, "reported once");
    }

    #[test]
    fn the_line_and_the_event_carry_the_same_fields() {
        let mut probe = ResumeProbe::start(0.0, tapped(), Item::Music, None);
        probe.playing(12.0);
        let report = probe.frame(20.0, Some(0.5)).expect("sounded");
        assert_eq!(
            report.line(),
            "pharos: resume click_to_playing_ms=12 playing_to_audible_ms=8 ready_state=4 \
             buffered_ahead_ms=180000 context_state=running base_latency_ms=10.0 \
             output_latency_ms=none context_resumed=false engine=direct item=music \
             paused_ms=none context_wake=none \
             user_agent=Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0"
        );
        let event = report.event();
        assert_eq!(event.kind, ClientEventKind::PlaybackResume);
        assert_eq!(event.level, ClientLevel::Info);
        assert_eq!(event.fields.len(), 13);
    }

    #[test]
    fn the_report_names_the_item_the_pause_and_the_wake() {
        let suspended = ResumeContext {
            audio: Some(AudioPath {
                context_state: "suspended".to_owned(),
                base_latency_ms: None,
                output_latency_ms: None,
            }),
            ..tapped()
        };
        let mut probe = ResumeProbe::start(400_000.0, suspended, Item::Video, Some(99_000.0));
        probe.playing(400_010.0);
        let report = probe
            .frame(400_030.0, Some(0.5))
            .expect("sounded")
            .with_wake(Wake::Resumed);
        let event = report.event();
        let field = |k: &str| {
            event
                .fields
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(field("item").as_deref(), Some("video"));
        assert_eq!(field("paused_ms").as_deref(), Some("301000"));
        assert_eq!(field("context_wake").as_deref(), Some("resumed"));
        assert_eq!(field("context_resumed").as_deref(), Some("true"));
        for (wake, label) in [
            (Wake::None, "none"),
            (Wake::Pending, "pending"),
            (Wake::Resumed, "resumed"),
            (Wake::Rejected, "rejected"),
            (Wake::Refused, "refused"),
        ] {
            assert_eq!(wake.label(), label);
        }
    }

    #[test]
    fn an_untapped_resume_still_times_the_click_to_playing() {
        let untapped = ResumeContext {
            audio: None,
            ..tapped()
        };
        let report = ResumeProbe::start(0.0, untapped, Item::Music, Some(0.0))
            .playing(3.0)
            .expect("nothing to wait for");
        assert_eq!(report.click_to_playing_ms, Some(3));
        assert_eq!(report.playing_to_audible, Audible::Untapped);
    }

    #[test]
    fn a_paused_song_rests_its_output_after_five_minutes() {
        assert!(!rests(1_000.0, 1_000.0));
        assert!(!rests(1_000.0, 300_999.0));
        assert!(rests(1_000.0, 301_000.0));
    }

    #[test]
    fn only_same_origin_and_mse_sources_are_routed() {
        let origin = "https://pharos.example";
        assert_eq!(
            routable("https://pharos.example/audio/1/universal?x=1", origin, false),
            Ok(())
        );
        assert_eq!(routable("https://pharos.example", origin, false), Ok(()));
        assert_eq!(
            routable("blob:https://pharos.example/0b1c", origin, false),
            Ok(())
        );
        let refused = |src: &str| routable(src, origin, false).expect_err(src);
        assert!(refused("https://cdn.example/a.mp3").contains("cross-origin source"));
        // A host that only starts with the origin is another origin.
        assert!(refused("https://pharos.example.evil/a.mp3").contains("cross-origin"));
        assert!(refused("https://pharos.example/v/main.m3u8?t=1").contains("native HLS"));
        assert!(
            routable("https://pharos.example/a.mp3", "", false).is_err(),
            "no origin, no route"
        );
    }

    #[test]
    fn a_cors_source_is_routed_and_a_refusal_keeps_its_query_out() {
        let origin = "https://pharos.example";
        assert_eq!(
            routable("https://media.example/audio/1/stream?api_key=t0k", origin, true),
            Ok(())
        );
        let refused = routable("https://media.example/a.mp3?api_key=t0k", origin, false)
            .expect_err("no CORS");
        assert!(!refused.contains("t0k"), "{refused}");
        let hls = routable("https://media.example/v/main.m3u8?api_key=t0k", origin, true)
            .expect_err("native HLS");
        assert!(hls.contains("native HLS") && !hls.contains("t0k"), "{hls}");
    }

    #[test]
    fn only_a_suspended_context_is_woken_and_the_report_says_so() {
        assert!(wakes_context("suspended"));
        // WebKit's state while a call or another app holds the audio session.
        assert!(wakes_context("interrupted"));
        assert!(!wakes_context("running"));
        assert!(!wakes_context("closed"));
        let suspended = ResumeContext {
            audio: Some(AudioPath {
                context_state: "suspended".to_owned(),
                base_latency_ms: None,
                output_latency_ms: None,
            }),
            ..tapped()
        };
        let mut probe = ResumeProbe::start(0.0, suspended, Item::Music, None);
        probe.playing(5.0);
        let report = probe.frame(15.0, Some(0.5)).expect("sounded");
        assert!(
            report.line().contains("context_resumed=true"),
            "{}",
            report.line()
        );
    }

    // B1141 / V1101: a tap says how its context started, with the values
    // behind the verdict; only a context that runs is not a fault.
    #[test]
    fn a_tap_start_names_its_outcome_and_values() {
        let started = TapStart::Started {
            state: "running".to_owned(),
            start_ms: 140,
        };
        assert_eq!(
            started.line(),
            "pharos: visualiser tap context=started state=running start_ms=140"
        );
        assert!(!started.is_fault());

        let stuck = TapStart::Stuck {
            state: "suspended".to_owned(),
        };
        assert_eq!(
            stuck.line(),
            "pharos: visualiser tap context=stuck state=suspended waited_ms=3000"
        );
        assert!(stuck.is_fault());

        let rejected = TapStart::Rejected {
            error: "NotAllowedError".to_owned(),
            state: "suspended".to_owned(),
        };
        assert!(rejected
            .line()
            .contains("context=rejected state=suspended error=NotAllowedError"));
        assert!(rejected.is_fault());

        let refused = TapStart::Refused {
            error: "InvalidStateError".to_owned(),
        };
        assert!(refused
            .line()
            .contains("context=refused error=InvalidStateError"));
        assert!(refused.is_fault());

        // Settled but not running (a browser that resolves early) is a fault.
        let odd = TapStart::Started {
            state: "suspended".to_owned(),
            start_ms: 5,
        };
        assert!(odd.is_fault());

        let labels = [
            started.label(),
            stuck.label(),
            rejected.label(),
            refused.label(),
        ];
        let mut distinct = labels.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), labels.len(), "{labels:?}");
    }
}
