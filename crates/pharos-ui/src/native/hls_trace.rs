//! What the Rust HLS engine tells telemetry (spec 031, 016 US4): start time,
//! seeks from `seeking` to playing, stalls, buffer levels and a rolled-up
//! window of segment downloads.
//!
//! Pure: the MSE adapter (`hls_web.rs`) feeds it `<video>` signals and
//! appended segments with the time, and sends what it returns. One client
//! event goes out per seek, stall and start, and one summary per window, so a
//! long play stays far inside the `Telemetry.submit` rate limit; per-segment
//! detail stays in the summary's numbers.

use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel};
use pharos_hls::planner::Track;

/// A summary goes out at least this often while segments arrive (V692).
pub const SUMMARY_INTERVAL_MS: u64 = 300_000;
/// A seek slower than this to playing is reported at warn, not info.
pub const SLOW_SEEK_MS: u64 = 3_000;
/// A start slower than this to playing is reported at warn, not info.
pub const SLOW_START_MS: u64 = 5_000;
/// `HTMLMediaElement.HAVE_FUTURE_DATA`: a `seeked` at this state plays on
/// without a `playing` event, so the seek is over at `seeked`.
const HAVE_FUTURE_DATA: u16 = 3;
/// Play head movement past the seek target that counts as playing again,
/// for a browser that resumes without firing `playing`.
const RESUMED_MS: u64 = 250;
/// Time-to-first-byte samples one window keeps for its median (the maximum
/// counts every segment).
const MAX_TTFB_SAMPLES: usize = 512;

/// A `<video>` event the adapter forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Playing,
    Waiting,
    Seeking,
    Seeked,
    Pause,
    TimeUpdate,
}

/// The `<video>` and its buffers when a signal fired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Clip {
    pub now_ms: u64,
    pub position_ms: u64,
    pub paused: bool,
    pub ready_state: u16,
    pub network_state: u16,
    /// Media buffered ahead of the play head on the track with the least.
    pub forward_ms: u64,
    /// The video track's buffered ranges, `start-end` in ms, comma separated.
    pub buffered: String,
}

/// One appended media segment, with the download that brought it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    pub track: Track,
    pub index: usize,
    pub bytes: u64,
    pub ttfb_ms: u64,
    pub load_ms: u64,
}

/// Why a tracked span ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The video played.
    Playing,
    /// The viewer paused before it played.
    Paused,
    /// A seek ended it.
    Seek,
    /// The player closed, or the engine failed or fell back, first.
    Abandoned,
}

impl End {
    pub fn label(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Seek => "seek",
            Self::Abandoned => "abandoned",
        }
    }
}

/// Why a summary went out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flush {
    Interval,
    Closed,
    Fatal,
    Fallback,
}

impl Flush {
    pub fn label(self) -> &'static str {
        match self {
            Self::Interval => "interval",
            Self::Closed => "closed",
            Self::Fatal => "fatal",
            Self::Fallback => "fallback",
        }
    }
}

/// The first video segment that landed after a seek: the split between the
/// download and the browser's decode in a slow seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstSegment {
    pub index: usize,
    pub ttfb_ms: u64,
    pub load_ms: u64,
    /// From `seeking` to this segment's append finishing.
    pub appended_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartReport {
    pub startup_ms: u64,
    pub start_ms: u64,
    pub end: End,
    pub segments: u32,
    pub first: Option<FirstSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekReport {
    pub from_ms: u64,
    pub to_ms: u64,
    pub latency_ms: u64,
    pub seeked_ms: Option<u64>,
    pub in_buffer: bool,
    /// Seeks before this one that were still running when it began (a scrub).
    pub superseded: u32,
    pub paused: bool,
    pub end: End,
    pub segments: u32,
    pub bytes: u64,
    pub first: Option<FirstSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallReport {
    pub duration_ms: u64,
    pub position_ms: u64,
    pub forward_ms: u64,
    pub buffered: String,
    pub network_state: u16,
    pub end: End,
    /// The last video segment's download before the stall began.
    pub last_ttfb_ms: Option<u64>,
    pub last_load_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryReport {
    pub reason: Option<Flush>,
    pub window_ms: u64,
    pub video_segments: u32,
    pub audio_segments: u32,
    pub bytes: u64,
    /// Video bytes over video download time, first byte to last: audio
    /// downloads run alongside, so their time would double count.
    pub kbps: Option<u64>,
    pub ttfb_p50_ms: Option<u64>,
    pub ttfb_max_ms: Option<u64>,
    pub load_max_ms: Option<u64>,
    pub buffer_min_ms: Option<u64>,
    pub buffer_mean_ms: Option<u64>,
    pub stalls: u32,
    pub stall_ms: u64,
    pub seeks: u32,
    pub seek_max_ms: Option<u64>,
    pub switches: u32,
    pub refused_switches: u32,
    pub quota: u32,
}

/// What the tracker has to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Start(StartReport),
    Seek(SeekReport),
    Stall(StallReport),
    Summary(SummaryReport),
}

struct Seeking {
    from_ms: u64,
    to_ms: u64,
    started_ms: u64,
    in_buffer: bool,
    superseded: u32,
    seeked_ms: Option<u64>,
    segments: u32,
    bytes: u64,
    first: Option<FirstSegment>,
}

struct Stalled {
    started_ms: u64,
    position_ms: u64,
    forward_ms: u64,
    buffered: String,
    network_state: u16,
    last_ttfb_ms: Option<u64>,
    last_load_ms: Option<u64>,
}

#[derive(Default)]
struct Window {
    started_ms: u64,
    video_segments: u32,
    audio_segments: u32,
    bytes: u64,
    video_bytes: u64,
    load_ms: u64,
    ttfbs: Vec<u64>,
    ttfb_max_ms: Option<u64>,
    load_max_ms: Option<u64>,
    buffer_min_ms: Option<u64>,
    buffer_sum_ms: u64,
    buffer_samples: u64,
    stalls: u32,
    stall_ms: u64,
    seeks: u32,
    seek_max_ms: Option<u64>,
    switches: u32,
    refused_switches: u32,
    quota: u32,
}

/// One engine session's playback, from attach to drop.
pub struct PlayTrace {
    attached_ms: u64,
    start_ms: u64,
    started: bool,
    start_segments: u32,
    start_first: Option<FirstSegment>,
    last_position_ms: u64,
    last_paused: bool,
    seeking: Option<Seeking>,
    stalled: Option<Stalled>,
    last_video: Option<(u64, u64)>,
    window: Window,
    finished: bool,
}

impl PlayTrace {
    pub fn new(now_ms: u64, start_ms: u64) -> Self {
        Self {
            attached_ms: now_ms,
            start_ms,
            started: false,
            start_segments: 0,
            start_first: None,
            last_position_ms: start_ms,
            last_paused: false,
            seeking: None,
            stalled: None,
            last_video: None,
            window: Window {
                started_ms: now_ms,
                ..Window::default()
            },
            finished: false,
        }
    }

    /// A `<video>` event; returns the spans it ended.
    pub fn observe(&mut self, signal: Signal, clip: &Clip) -> Vec<Report> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.last_paused = clip.paused;
        match signal {
            Signal::Seeking => {
                if let Some(stall) = self.stalled.take() {
                    out.push(self.end_stall(stall, End::Seek, clip.now_ms));
                }
                let superseded = self.seeking.take().map_or(0, |s| s.superseded + 1);
                let from_ms = self.last_position_ms;
                self.seeking = Some(Seeking {
                    from_ms,
                    to_ms: clip.position_ms,
                    started_ms: clip.now_ms,
                    in_buffer: clip.forward_ms > 0,
                    superseded,
                    seeked_ms: None,
                    segments: 0,
                    bytes: 0,
                    first: None,
                });
            }
            Signal::Seeked => {
                if let Some(seek) = self.seeking.as_mut() {
                    seek.seeked_ms = Some(clip.now_ms.saturating_sub(seek.started_ms));
                    if clip.paused {
                        out.extend(self.end_seek(End::Paused, clip));
                    } else if clip.ready_state >= HAVE_FUTURE_DATA {
                        out.extend(self.end_seek(End::Playing, clip));
                    }
                }
                self.last_position_ms = clip.position_ms;
            }
            Signal::Playing => {
                if !self.started {
                    out.push(self.end_start(End::Playing, clip.now_ms));
                }
                if self.seeking.is_some() {
                    out.extend(self.end_seek(End::Playing, clip));
                }
                if let Some(stall) = self.stalled.take() {
                    out.push(self.end_stall(stall, End::Playing, clip.now_ms));
                }
            }
            Signal::Waiting => {
                // Waiting before the first frame is the start, and after a seek
                // is the seek: neither is a stall.
                if self.started && self.seeking.is_none() && self.stalled.is_none() && !clip.paused
                {
                    self.stalled = Some(Stalled {
                        started_ms: clip.now_ms,
                        position_ms: clip.position_ms,
                        forward_ms: clip.forward_ms,
                        buffered: clip.buffered.clone(),
                        network_state: clip.network_state,
                        last_ttfb_ms: self.last_video.map(|(t, _)| t),
                        last_load_ms: self.last_video.map(|(_, l)| l),
                    });
                }
            }
            Signal::Pause => {
                if let Some(stall) = self.stalled.take() {
                    out.push(self.end_stall(stall, End::Paused, clip.now_ms));
                }
            }
            Signal::TimeUpdate => {
                let moving = !clip.paused;
                if let Some(seek) = self.seeking.as_ref() {
                    if moving
                        && seek.seeked_ms.is_some()
                        && clip.position_ms >= seek.to_ms.saturating_add(RESUMED_MS)
                    {
                        out.extend(self.end_seek(End::Playing, clip));
                    }
                } else {
                    if let Some(stall) = self.stalled.as_ref() {
                        if moving
                            && clip.position_ms >= stall.position_ms.saturating_add(RESUMED_MS)
                        {
                            if let Some(stall) = self.stalled.take() {
                                out.push(self.end_stall(stall, End::Playing, clip.now_ms));
                            }
                        }
                    } else if moving && self.started {
                        self.sample_buffer(clip.forward_ms);
                    }
                    self.last_position_ms = clip.position_ms;
                }
            }
        }
        out
    }

    /// A media segment finished appending.
    pub fn appended(&mut self, seg: Appended, now_ms: u64) -> Option<Report> {
        if self.finished {
            return None;
        }
        let w = &mut self.window;
        w.bytes = w.bytes.saturating_add(seg.bytes);
        match seg.track {
            Track::Video => {
                w.video_segments = w.video_segments.saturating_add(1);
                w.video_bytes = w.video_bytes.saturating_add(seg.bytes);
                w.load_ms = w.load_ms.saturating_add(seg.load_ms);
                if w.ttfbs.len() < MAX_TTFB_SAMPLES {
                    w.ttfbs.push(seg.ttfb_ms);
                }
                w.ttfb_max_ms = Some(w.ttfb_max_ms.map_or(seg.ttfb_ms, |m| m.max(seg.ttfb_ms)));
                w.load_max_ms = Some(w.load_max_ms.map_or(seg.load_ms, |m| m.max(seg.load_ms)));
                self.last_video = Some((seg.ttfb_ms, seg.load_ms));
            }
            Track::Audio => w.audio_segments = w.audio_segments.saturating_add(1),
        }
        if !self.started {
            self.start_segments = self.start_segments.saturating_add(1);
            if seg.track == Track::Video && self.start_first.is_none() {
                self.start_first = Some(FirstSegment {
                    index: seg.index,
                    ttfb_ms: seg.ttfb_ms,
                    load_ms: seg.load_ms,
                    appended_ms: now_ms.saturating_sub(self.attached_ms),
                });
            }
        }
        if let Some(seek) = self.seeking.as_mut() {
            seek.segments = seek.segments.saturating_add(1);
            seek.bytes = seek.bytes.saturating_add(seg.bytes);
            if seg.track == Track::Video && seek.first.is_none() {
                seek.first = Some(FirstSegment {
                    index: seg.index,
                    ttfb_ms: seg.ttfb_ms,
                    load_ms: seg.load_ms,
                    appended_ms: now_ms.saturating_sub(seek.started_ms),
                });
            }
        }
        (now_ms.saturating_sub(self.window.started_ms) >= SUMMARY_INTERVAL_MS)
            .then(|| Report::Summary(self.flush_window(Flush::Interval, now_ms)))
    }

    /// The ABR rule picked a rung; `applied` is false when the planner refused it.
    pub fn switched(&mut self, applied: bool) {
        if applied {
            self.window.switches = self.window.switches.saturating_add(1);
        } else {
            self.window.refused_switches = self.window.refused_switches.saturating_add(1);
        }
    }

    /// A `QuotaExceededError` lowered the forward target.
    pub fn quota_exceeded(&mut self) {
        self.window.quota = self.window.quota.saturating_add(1);
    }

    /// The engine is going: end what is still open as abandoned and send the
    /// last window. Later calls return nothing.
    pub fn finish(&mut self, reason: Flush, now_ms: u64) -> Vec<Report> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.started {
            out.push(self.end_start(End::Abandoned, now_ms));
        }
        if let Some(seek) = self.seeking.take() {
            let report = Self::seek_report(seek, End::Abandoned, self.last_paused, now_ms);
            self.count_seek(&report);
            out.push(report);
        }
        if let Some(stall) = self.stalled.take() {
            out.push(self.end_stall(stall, End::Abandoned, now_ms));
        }
        out.push(Report::Summary(self.flush_window(reason, now_ms)));
        self.finished = true;
        out
    }

    fn sample_buffer(&mut self, forward_ms: u64) {
        let w = &mut self.window;
        w.buffer_min_ms = Some(w.buffer_min_ms.map_or(forward_ms, |m| m.min(forward_ms)));
        w.buffer_sum_ms = w.buffer_sum_ms.saturating_add(forward_ms);
        w.buffer_samples = w.buffer_samples.saturating_add(1);
    }

    fn end_start(&mut self, end: End, now_ms: u64) -> Report {
        self.started = true;
        Report::Start(StartReport {
            startup_ms: now_ms.saturating_sub(self.attached_ms),
            start_ms: self.start_ms,
            end,
            segments: self.start_segments,
            first: self.start_first,
        })
    }

    fn end_seek(&mut self, end: End, clip: &Clip) -> Option<Report> {
        let seek = self.seeking.take()?;
        self.last_position_ms = clip.position_ms;
        let report = Self::seek_report(seek, end, clip.paused, clip.now_ms);
        self.count_seek(&report);
        Some(report)
    }

    fn count_seek(&mut self, report: &Report) {
        if let Report::Seek(s) = report {
            let w = &mut self.window;
            w.seeks = w.seeks.saturating_add(1);
            w.seek_max_ms = Some(w.seek_max_ms.map_or(s.latency_ms, |m| m.max(s.latency_ms)));
        }
    }

    fn seek_report(seek: Seeking, end: End, paused: bool, now_ms: u64) -> Report {
        Report::Seek(SeekReport {
            from_ms: seek.from_ms,
            to_ms: seek.to_ms,
            latency_ms: now_ms.saturating_sub(seek.started_ms),
            seeked_ms: seek.seeked_ms,
            in_buffer: seek.in_buffer,
            superseded: seek.superseded,
            paused,
            end,
            segments: seek.segments,
            bytes: seek.bytes,
            first: seek.first,
        })
    }

    fn end_stall(&mut self, stall: Stalled, end: End, now_ms: u64) -> Report {
        let duration_ms = now_ms.saturating_sub(stall.started_ms);
        let w = &mut self.window;
        w.stalls = w.stalls.saturating_add(1);
        w.stall_ms = w.stall_ms.saturating_add(duration_ms);
        Report::Stall(StallReport {
            duration_ms,
            position_ms: stall.position_ms,
            forward_ms: stall.forward_ms,
            buffered: stall.buffered,
            network_state: stall.network_state,
            end,
            last_ttfb_ms: stall.last_ttfb_ms,
            last_load_ms: stall.last_load_ms,
        })
    }

    fn flush_window(&mut self, reason: Flush, now_ms: u64) -> SummaryReport {
        let w = std::mem::replace(
            &mut self.window,
            Window {
                started_ms: now_ms,
                ..Window::default()
            },
        );
        let mut ttfbs = w.ttfbs;
        ttfbs.sort_unstable();
        SummaryReport {
            reason: Some(reason),
            window_ms: now_ms.saturating_sub(w.started_ms),
            video_segments: w.video_segments,
            audio_segments: w.audio_segments,
            bytes: w.bytes,
            kbps: (w.load_ms > 0).then(|| w.video_bytes.saturating_mul(8) / w.load_ms),
            ttfb_p50_ms: ttfbs.get(ttfbs.len() / 2).copied(),
            ttfb_max_ms: w.ttfb_max_ms,
            load_max_ms: w.load_max_ms,
            buffer_min_ms: w.buffer_min_ms,
            buffer_mean_ms: (w.buffer_samples > 0).then(|| w.buffer_sum_ms / w.buffer_samples),
            stalls: w.stalls,
            stall_ms: w.stall_ms,
            seeks: w.seeks,
            seek_max_ms: w.seek_max_ms,
            switches: w.switches,
            refused_switches: w.refused_switches,
            quota: w.quota,
        }
    }
}

fn opt(v: Option<u64>) -> String {
    v.map_or_else(|| "none".to_owned(), |v| v.to_string())
}

fn first_fields(first: Option<FirstSegment>) -> [(&'static str, String); 4] {
    [
        (
            "first_index",
            first.map_or_else(|| "none".to_owned(), |f| f.index.to_string()),
        ),
        ("first_ttfb_ms", opt(first.map(|f| f.ttfb_ms))),
        ("first_load_ms", opt(first.map(|f| f.load_ms))),
        ("first_appended_ms", opt(first.map(|f| f.appended_ms))),
    ]
}

fn line_of(prefix: &str, fields: &[(&'static str, String)]) -> String {
    let mut line = format!("pharos: playback {prefix} engine=rust");
    for (k, v) in fields {
        line.push(' ');
        line.push_str(k);
        line.push('=');
        line.push_str(v);
    }
    line
}

impl Report {
    fn parts(
        &self,
    ) -> (
        &'static str,
        ClientEventKind,
        ClientLevel,
        Vec<(&'static str, String)>,
    ) {
        match self {
            Self::Start(r) => {
                let mut f = vec![
                    ("startup_ms", r.startup_ms.to_string()),
                    ("start_ms", r.start_ms.to_string()),
                    ("end", r.end.label().to_owned()),
                    ("segments", r.segments.to_string()),
                ];
                f.extend(first_fields(r.first));
                let level = if r.end == End::Playing && r.startup_ms <= SLOW_START_MS {
                    ClientLevel::Info
                } else {
                    ClientLevel::Warn
                };
                ("start", ClientEventKind::PlaybackStart, level, f)
            }
            Self::Seek(r) => {
                let mut f = vec![
                    ("from_ms", r.from_ms.to_string()),
                    ("to_ms", r.to_ms.to_string()),
                    ("latency_ms", r.latency_ms.to_string()),
                    ("seeked_ms", opt(r.seeked_ms)),
                    ("in_buffer", r.in_buffer.to_string()),
                    ("superseded", r.superseded.to_string()),
                    ("paused", r.paused.to_string()),
                    ("end", r.end.label().to_owned()),
                    ("segments", r.segments.to_string()),
                    ("bytes", r.bytes.to_string()),
                ];
                f.extend(first_fields(r.first));
                let level = if r.end == End::Abandoned || r.latency_ms > SLOW_SEEK_MS {
                    ClientLevel::Warn
                } else {
                    ClientLevel::Info
                };
                ("seek", ClientEventKind::Seek, level, f)
            }
            Self::Stall(r) => (
                "stall",
                ClientEventKind::PlaybackStall,
                ClientLevel::Warn,
                vec![
                    ("duration_ms", r.duration_ms.to_string()),
                    ("position_ms", r.position_ms.to_string()),
                    ("forward_ms", r.forward_ms.to_string()),
                    ("buffered_ranges", r.buffered.clone()),
                    ("network_state", r.network_state.to_string()),
                    ("end", r.end.label().to_owned()),
                    ("last_ttfb_ms", opt(r.last_ttfb_ms)),
                    ("last_load_ms", opt(r.last_load_ms)),
                ],
            ),
            Self::Summary(r) => {
                let level = if r.stalls > 0 || r.quota > 0 || r.refused_switches > 0 {
                    ClientLevel::Warn
                } else {
                    ClientLevel::Info
                };
                (
                    "summary",
                    ClientEventKind::PlaybackSummary,
                    level,
                    vec![
                        ("reason", r.reason.map_or("none", Flush::label).to_owned()),
                        ("window_ms", r.window_ms.to_string()),
                        ("video_segments", r.video_segments.to_string()),
                        ("audio_segments", r.audio_segments.to_string()),
                        ("bytes", r.bytes.to_string()),
                        ("kbps", opt(r.kbps)),
                        ("ttfb_p50_ms", opt(r.ttfb_p50_ms)),
                        ("ttfb_max_ms", opt(r.ttfb_max_ms)),
                        ("load_max_ms", opt(r.load_max_ms)),
                        ("buffer_min_ms", opt(r.buffer_min_ms)),
                        ("buffer_mean_ms", opt(r.buffer_mean_ms)),
                        ("stalls", r.stalls.to_string()),
                        ("stall_ms", r.stall_ms.to_string()),
                        ("seeks", r.seeks.to_string()),
                        ("seek_max_ms", opt(r.seek_max_ms)),
                        ("switches", r.switches.to_string()),
                        ("refused_switches", r.refused_switches.to_string()),
                        ("quota", r.quota.to_string()),
                    ],
                )
            }
        }
    }

    /// The console line (V690).
    pub fn line(&self) -> String {
        let (prefix, _, _, fields) = self.parts();
        line_of(prefix, &fields)
    }

    /// Info lines go to `console.info`, the rest to `console.warn`.
    pub fn is_warn(&self) -> bool {
        !matches!(self.parts().2, ClientLevel::Info)
    }

    /// The client event, with the same fields as the line (V690).
    pub fn event(&self, route: String) -> ClientEvent {
        let (_, kind, level, fields) = self.parts();
        let mut all = Vec::with_capacity(fields.len() + 1);
        all.push(("engine", "rust".to_owned()));
        all.extend(fields);
        ClientEvent {
            kind,
            level,
            route,
            fields: all,
        }
    }
}

/// `start-end` pairs in ms, comma separated: the shape a stall reports its
/// buffered ranges in.
pub fn ranges_label(ranges: &[(u64, u64)]) -> String {
    let mut out = String::new();
    for (i, (start, end)) in ranges.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{start}-{end}"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn clip(now_ms: u64, position_ms: u64) -> Clip {
        Clip {
            now_ms,
            position_ms,
            paused: false,
            ready_state: 4,
            network_state: 2,
            forward_ms: 10_000,
            buffered: "0-20000".to_owned(),
        }
    }

    fn video(index: usize, ttfb_ms: u64, load_ms: u64) -> Appended {
        Appended {
            track: Track::Video,
            index,
            bytes: 100_000,
            ttfb_ms,
            load_ms,
        }
    }

    fn started(now_ms: u64) -> PlayTrace {
        let mut t = PlayTrace::new(now_ms, 0);
        t.observe(Signal::Playing, &clip(now_ms, 0));
        t
    }

    fn field<'a>(event: &'a ClientEvent, key: &str) -> &'a str {
        event
            .fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no field {key} in {:?}", event.fields))
    }

    #[test]
    fn start_is_reported_at_the_first_playing_with_its_first_segment() {
        let mut t = PlayTrace::new(1_000, 42_000);
        assert!(t.appended(video(7, 300, 200), 1_600).is_none());
        t.observe(Signal::Waiting, &clip(1_700, 42_000));
        let out = t.observe(Signal::Playing, &clip(2_200, 42_000));
        assert_eq!(
            out,
            vec![Report::Start(StartReport {
                startup_ms: 1_200,
                start_ms: 42_000,
                end: End::Playing,
                segments: 1,
                first: Some(FirstSegment {
                    index: 7,
                    ttfb_ms: 300,
                    load_ms: 200,
                    appended_ms: 600
                }),
            })]
        );
        // A second `playing` is not a second start.
        assert!(t.observe(Signal::Playing, &clip(9_000, 49_000)).is_empty());
    }

    #[test]
    fn a_seek_runs_from_seeking_to_playing_and_names_where_it_came_from() {
        let mut t = started(0);
        t.observe(Signal::TimeUpdate, &clip(5_000, 5_000));
        let mut at_target = clip(5_100, 600_000);
        at_target.forward_ms = 0;
        assert!(t.observe(Signal::Seeking, &at_target).is_empty());
        // Waiting after a seek is the seek, not a stall.
        assert!(t.observe(Signal::Waiting, &at_target).is_empty());
        t.appended(video(100, 900, 400), 6_600);
        let mut seeked = clip(6_800, 600_000);
        seeked.ready_state = 2;
        assert!(t.observe(Signal::Seeked, &seeked).is_empty());
        let out = t.observe(Signal::Playing, &clip(7_100, 600_000));
        let Report::Seek(seek) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(seek.from_ms, 5_000);
        assert_eq!(seek.to_ms, 600_000);
        assert_eq!(seek.latency_ms, 2_000);
        assert_eq!(seek.seeked_ms, Some(1_700));
        assert!(!seek.in_buffer);
        assert_eq!(seek.end, End::Playing);
        assert_eq!(
            seek.first,
            Some(FirstSegment {
                index: 100,
                ttfb_ms: 900,
                load_ms: 400,
                appended_ms: 1_500
            })
        );
        assert_eq!(out.len(), 1, "no stall was reported: {out:?}");
    }

    #[test]
    fn a_seek_in_buffer_that_plays_on_ends_at_seeked() {
        let mut t = started(0);
        assert!(t.observe(Signal::Seeking, &clip(1_000, 8_000)).is_empty());
        let out = t.observe(Signal::Seeked, &clip(1_040, 8_000));
        let Report::Seek(seek) = &out[0] else {
            panic!("{out:?}")
        };
        assert!(seek.in_buffer);
        assert_eq!(seek.latency_ms, 40);
        assert_eq!(seek.end, End::Playing);
    }

    #[test]
    fn a_seek_while_paused_ends_at_seeked_as_paused() {
        let mut t = started(0);
        let mut c = clip(1_000, 8_000);
        c.paused = true;
        t.observe(Signal::Seeking, &c);
        c.now_ms = 1_500;
        c.ready_state = 1;
        let out = t.observe(Signal::Seeked, &c);
        let Report::Seek(seek) = &out[0] else {
            panic!("{out:?}")
        };
        assert!(seek.paused);
        assert_eq!(seek.end, End::Paused);
        assert_eq!(seek.latency_ms, 500);
    }

    #[test]
    fn a_seek_that_resumes_without_playing_ends_when_the_head_moves() {
        let mut t = started(0);
        t.observe(Signal::Seeking, &clip(1_000, 8_000));
        let mut s = clip(1_200, 8_000);
        s.ready_state = 2;
        t.observe(Signal::Seeked, &s);
        assert!(t
            .observe(Signal::TimeUpdate, &clip(1_300, 8_100))
            .is_empty());
        let out = t.observe(Signal::TimeUpdate, &clip(1_500, 8_300));
        assert!(matches!(&out[0], Report::Seek(s) if s.latency_ms == 500));
    }

    #[test]
    fn a_scrub_reports_once_with_the_seeks_it_replaced() {
        let mut t = started(0);
        t.observe(Signal::TimeUpdate, &clip(500, 500));
        t.observe(Signal::Seeking, &clip(1_000, 10_000));
        t.observe(Signal::Seeking, &clip(1_100, 20_000));
        t.observe(Signal::Seeking, &clip(1_200, 30_000));
        let out = t.observe(Signal::Seeked, &clip(1_300, 30_000));
        assert_eq!(out.len(), 1);
        let Report::Seek(seek) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!((seek.from_ms, seek.to_ms), (500, 30_000));
        assert_eq!(seek.superseded, 2);
        assert_eq!(seek.latency_ms, 100);
    }

    #[test]
    fn a_stall_runs_from_waiting_to_playing_with_the_buffer_it_ran_dry_on() {
        let mut t = started(0);
        t.appended(video(3, 2_500, 1_800), 900);
        let mut dry = clip(10_000, 9_900);
        dry.forward_ms = 0;
        dry.buffered = "0-9900".to_owned();
        dry.network_state = 2;
        assert!(t.observe(Signal::Waiting, &dry).is_empty());
        // A second waiting inside the stall does not restart it.
        assert!(t.observe(Signal::Waiting, &clip(10_500, 9_900)).is_empty());
        let out = t.observe(Signal::Playing, &clip(12_400, 9_900));
        assert_eq!(
            out,
            vec![Report::Stall(StallReport {
                duration_ms: 2_400,
                position_ms: 9_900,
                forward_ms: 0,
                buffered: "0-9900".to_owned(),
                network_state: 2,
                end: End::Playing,
                last_ttfb_ms: Some(2_500),
                last_load_ms: Some(1_800),
            })]
        );
    }

    #[test]
    fn waiting_while_paused_or_before_the_start_is_no_stall() {
        let mut t = PlayTrace::new(0, 0);
        t.observe(Signal::Waiting, &clip(100, 0));
        let out = t.observe(Signal::Playing, &clip(900, 0));
        assert!(matches!(out.as_slice(), [Report::Start(_)]), "{out:?}");
        let mut paused = clip(2_000, 1_000);
        paused.paused = true;
        t.observe(Signal::Waiting, &paused);
        assert!(t.observe(Signal::Playing, &clip(3_000, 1_000)).is_empty());
    }

    #[test]
    fn a_seek_or_pause_ends_a_stall_and_says_so() {
        let mut t = started(0);
        t.observe(Signal::Waiting, &clip(1_000, 900));
        let out = t.observe(Signal::Seeking, &clip(1_500, 50_000));
        assert!(matches!(&out[0], Report::Stall(s) if s.end == End::Seek && s.duration_ms == 500));
        t.observe(Signal::Seeked, &clip(1_600, 50_000));
        t.observe(Signal::Waiting, &clip(2_000, 50_100));
        let out = t.observe(Signal::Pause, &clip(2_300, 50_100));
        assert!(matches!(&out[0], Report::Stall(s) if s.end == End::Paused));
    }

    #[test]
    fn finish_reports_what_was_still_open_as_abandoned_then_nothing_more() {
        let mut t = started(0);
        t.observe(Signal::Waiting, &clip(1_000, 900));
        let out = t.finish(Flush::Closed, 4_000);
        assert!(
            matches!(&out[0], Report::Stall(s) if s.end == End::Abandoned && s.duration_ms == 3_000)
        );
        assert!(matches!(&out[1], Report::Summary(s) if s.stalls == 1 && s.stall_ms == 3_000));
        assert!(t.finish(Flush::Closed, 5_000).is_empty());
        assert!(t.observe(Signal::Playing, &clip(6_000, 900)).is_empty());

        // A seek the viewer gave up on: the slow-seek case.
        let mut t = started(0);
        t.observe(Signal::Seeking, &clip(1_000, 900_000));
        let out = t.finish(Flush::Closed, 9_000);
        let Report::Seek(seek) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!((seek.end, seek.latency_ms), (End::Abandoned, 8_000));
        assert!(!seek.paused);
        assert!(Report::Seek(seek.clone()).is_warn());
        // The summary counts the seek its line reported.
        assert!(
            matches!(&out[1], Report::Summary(s) if s.seeks == 1 && s.seek_max_ms == Some(8_000)),
            "{out:?}"
        );

        // A paused seek still open at close says it was paused.
        let mut t = started(0);
        let mut c = clip(1_000, 900_000);
        c.paused = true;
        t.observe(Signal::Seeking, &c);
        let out = t.finish(Flush::Closed, 2_000);
        assert!(matches!(&out[0], Report::Seek(s) if s.paused), "{out:?}");

        // A player closed before the first frame.
        let mut t = PlayTrace::new(0, 0);
        let out = t.finish(Flush::Fallback, 2_000);
        assert!(matches!(&out[0], Report::Start(s) if s.end == End::Abandoned));
        assert!(matches!(&out[1], Report::Summary(s) if s.reason == Some(Flush::Fallback)));
    }

    #[test]
    fn the_summary_rolls_up_downloads_buffer_and_events_per_window() {
        let mut t = started(0);
        for (i, ttfb) in [100, 300, 200, 900].into_iter().enumerate() {
            assert!(t.appended(video(i, ttfb, 500), 1_000 + i as u64).is_none());
        }
        t.appended(
            Appended {
                track: Track::Audio,
                index: 0,
                bytes: 20_000,
                ttfb_ms: 50,
                load_ms: 10,
            },
            1_100,
        );
        for (pos, ahead) in [(1_000, 8_000), (2_000, 4_000), (3_000, 6_000)] {
            let mut c = clip(pos, pos);
            c.forward_ms = ahead;
            t.observe(Signal::TimeUpdate, &c);
        }
        t.switched(true);
        t.switched(false);
        t.quota_exceeded();
        let Some(Report::Summary(s)) = t.appended(video(4, 100, 500), SUMMARY_INTERVAL_MS) else {
            panic!("no interval summary")
        };
        assert_eq!(s.reason, Some(Flush::Interval));
        assert_eq!((s.video_segments, s.audio_segments), (5, 1));
        assert_eq!(s.bytes, 520_000);
        // Video only: 5 segments of 100 KB over 2.5 s of video downloads.
        assert_eq!(s.kbps, Some(500_000 * 8 / 2_500));
        assert_eq!((s.ttfb_p50_ms, s.ttfb_max_ms), (Some(200), Some(900)));
        assert_eq!(
            (s.buffer_min_ms, s.buffer_mean_ms),
            (Some(4_000), Some(6_000))
        );
        assert_eq!((s.switches, s.refused_switches, s.quota), (1, 1, 1));
        // The next window starts empty.
        let out = t.finish(Flush::Closed, SUMMARY_INTERVAL_MS + 10);
        let Report::Summary(next) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!((next.video_segments, next.window_ms), (0, 10));
    }

    /// V690: the console line and the client event carry the same fields,
    /// the event under the kind the 016 catalogue names, with `engine=rust`.
    #[test]
    fn line_and_event_carry_the_same_fields() {
        let mut t = started(0);
        t.observe(Signal::Waiting, &clip(1_000, 900));
        let out = t.finish(Flush::Fatal, 2_000);
        let kinds: Vec<_> = out.iter().map(|r| r.event(String::new()).kind).collect();
        assert_eq!(
            kinds,
            [
                ClientEventKind::PlaybackStall,
                ClientEventKind::PlaybackSummary
            ]
        );
        for report in &out {
            let event = report.event("/watch/x".to_owned());
            assert_eq!(field(&event, "engine"), "rust");
            let line = report.line();
            assert!(line.contains("engine=rust"), "{line}");
            for (k, v) in &event.fields {
                assert!(
                    line.contains(&format!("{k}={v}")),
                    "{k}={v} missing from {line}"
                );
            }
        }
        let start = PlayTrace::new(0, 0).finish(Flush::Closed, 1)[0].event(String::new());
        assert_eq!(start.kind, ClientEventKind::PlaybackStart);
        assert_eq!(field(&start, "end"), "abandoned");
    }

    #[test]
    fn the_worst_first_byte_counts_past_the_median_samples() {
        let mut t = started(0);
        for i in 0..MAX_TTFB_SAMPLES + 10 {
            t.appended(video(i, 100, 50), 1_000);
        }
        t.appended(video(9_999, 2_500, 50), 1_000);
        let out = t.finish(Flush::Closed, 2_000);
        let Report::Summary(s) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!((s.ttfb_p50_ms, s.ttfb_max_ms), (Some(100), Some(2_500)));
    }

    #[test]
    fn labels_are_distinct() {
        use std::collections::HashSet;
        let ends = [End::Playing, End::Paused, End::Seek, End::Abandoned].map(End::label);
        assert_eq!(ends.iter().collect::<HashSet<_>>().len(), ends.len());
        let flushes = [
            Flush::Interval,
            Flush::Closed,
            Flush::Fatal,
            Flush::Fallback,
        ]
        .map(Flush::label);
        assert_eq!(flushes.iter().collect::<HashSet<_>>().len(), flushes.len());
    }

    #[test]
    fn ranges_label_joins_pairs() {
        assert_eq!(ranges_label(&[]), "");
        assert_eq!(
            ranges_label(&[(0, 6_000), (30_000, 36_000)]),
            "0-6000,30000-36000"
        );
    }
}
