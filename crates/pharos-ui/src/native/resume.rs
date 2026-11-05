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

/// Whether a play wakes a tap's `AudioContext` in `state` first: only
/// a suspended one, or one WebKit interrupted (a call or another app took
/// the audio session); a closed one cannot be resumed.
pub fn wakes_context(state: &str) -> bool {
    matches!(state, "suspended" | "interrupted")
}

/// The Web Audio path the song is routed through, when it is tapped.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioPath {
    /// `AudioContext.state` at the resume: running, suspended or closed.
    pub context_state: String,
    pub base_latency_ms: Option<f64>,
    pub output_latency_ms: Option<f64>,
}

/// The media element as the resume found it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeContext {
    /// `HTMLMediaElement.readyState`, 0 to 4.
    pub ready_state: u16,
    /// Buffered media ahead of the playhead.
    pub buffered_ahead_ms: u64,
    /// `direct`, `hlsjs` (MSE) or `native_hls`.
    pub engine: &'static str,
    pub muted: bool,
    /// `None` when the element is not routed through an analyser.
    pub audio: Option<AudioPath>,
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
        ]
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
    done: bool,
}

fn whole_ms(from: f64, to: f64) -> u64 {
    // Never negative; lossless below 2^53 ms.
    (to - from).max(0.0).round() as u64
}

impl ResumeProbe {
    pub fn start(now_ms: f64, context: ResumeContext) -> Self {
        Self {
            started_ms: now_ms,
            playing_ms: None,
            sounded_ms: None,
            context,
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
        }
    }

    #[test]
    fn a_resume_is_timed_to_playing_and_then_to_sound() {
        let mut probe = ResumeProbe::start(1_000.0, tapped());
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
        let mut probe = ResumeProbe::start(0.0, tapped());
        assert_eq!(probe.frame(10.0, Some(0.3)), None);
        let report = probe.playing(30.0).expect("playing ends it");
        assert_eq!(report.click_to_playing_ms, Some(30));
        assert_eq!(report.playing_to_audible, Audible::Ms(0));
    }

    #[test]
    fn no_sound_within_the_limit_is_a_timeout() {
        let mut probe = ResumeProbe::start(0.0, tapped());
        probe.playing(100.0);
        assert_eq!(probe.frame(4_999.0, Some(0.0)), None);
        let report = probe.frame(5_000.0, Some(0.0)).expect("timed out");
        assert_eq!(report.click_to_playing_ms, Some(100));
        assert_eq!(report.playing_to_audible, Audible::Timeout);
        assert_eq!(report.event().level, ClientLevel::Warn);
    }

    #[test]
    fn no_playing_within_the_limit_is_a_timeout_too() {
        let mut probe = ResumeProbe::start(0.0, tapped());
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
        let report = ResumeProbe::start(0.0, untapped)
            .playing(70.0)
            .expect("nothing to wait for");
        assert_eq!(report.playing_to_audible, Audible::Untapped);
        assert!(report.line().contains("context_state=untapped"));

        let muted = ResumeContext {
            muted: true,
            ..tapped()
        };
        let report = ResumeProbe::start(0.0, muted).playing(70.0).expect("muted");
        assert_eq!(report.playing_to_audible, Audible::Muted);
    }

    #[test]
    fn a_pause_before_sound_still_reports() {
        let mut probe = ResumeProbe::start(0.0, tapped());
        probe.playing(50.0);
        let report = probe.paused().expect("paused ends it");
        assert_eq!(report.playing_to_audible, Audible::Paused);
        assert_eq!(probe.paused(), None);
    }

    #[test]
    fn a_pause_or_a_second_play_before_playing_is_not_a_timeout() {
        let report = ResumeProbe::start(0.0, tapped()).paused().expect("paused");
        assert!(
            report.line().contains("click_to_playing_ms=none"),
            "{}",
            report.line()
        );
        assert_eq!(report.event().level, ClientLevel::Info);

        let mut probe = ResumeProbe::start(0.0, tapped());
        let report = probe.superseded().expect("superseded");
        assert_eq!(report.playing_to_audible, Audible::Superseded);
        assert!(report.line().contains("playing_to_audible_ms=superseded"));
        assert_eq!(report.event().level, ClientLevel::Info);
        assert_eq!(probe.superseded(), None, "reported once");
    }

    #[test]
    fn the_line_and_the_event_carry_the_same_fields() {
        let mut probe = ResumeProbe::start(0.0, tapped());
        probe.playing(12.0);
        let report = probe.frame(20.0, Some(0.5)).expect("sounded");
        assert_eq!(
            report.line(),
            "pharos: resume click_to_playing_ms=12 playing_to_audible_ms=8 ready_state=4 \
             buffered_ahead_ms=180000 context_state=running base_latency_ms=10.0 \
             output_latency_ms=none context_resumed=false engine=direct"
        );
        let event = report.event();
        assert_eq!(event.kind, ClientEventKind::PlaybackResume);
        assert_eq!(event.level, ClientLevel::Info);
        assert_eq!(event.fields.len(), 9);
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
        let mut probe = ResumeProbe::start(0.0, suspended);
        probe.playing(5.0);
        let report = probe.frame(15.0, Some(0.5)).expect("sounded");
        assert!(
            report.line().contains("context_resumed=true"),
            "{}",
            report.line()
        );
    }
}
