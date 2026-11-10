//! What the director is told about the audio and the video (spec 028 FR-009).
//!
//! These mirror the server's signal pass field for field, so the server
//! converts with a plain copy and this crate stays free of the media crates.

/// Audio measurements of one subtitle line's span. A field is `None` when
/// the span has no audio or the measure could not be made.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineSignals {
    /// EBU R128 loudness of the span, LUFS.
    pub loudness_lufs: Option<f32>,
    /// Sample peak of the span, dBFS.
    pub peak_dbfs: Option<f32>,
    /// Median F0 of the voiced frames, Hz.
    pub pitch_hz: Option<f32>,
    /// Voiced frames over frames in the span, 0.0 when there are none.
    pub voiced_ratio: f32,
    /// Share of the span's energy above 3.4 kHz, 0.0 to 1.0.
    pub high_band_share: Option<f32>,
}

impl LineSignals {
    /// A span with no audio.
    pub const NONE: LineSignals = LineSignals {
        loudness_lufs: None,
        peak_dbfs: None,
        pitch_hz: None,
        voiced_ratio: 0.0,
        high_band_share: None,
    };
}

/// The signals for a whole track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackSignals {
    /// One entry per source line, in order. A line past the end of this list
    /// is treated as having no audio.
    pub lines: Vec<LineSignals>,
    /// Likely scene cuts, ms from the start of the file.
    pub cuts_ms: Vec<u64>,
    /// Per source line: the height of text burned into the bottom of the
    /// picture under it, as a share of the frame height (0.0 for none);
    /// `None`, or a line past the end, when nothing was checked.
    pub bottom_text: Vec<Option<f32>>,
}

/// What a stretch of the episode is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentKind {
    Intro,
    Outro,
    Recap,
    Other,
}

/// A labelled stretch of the episode, from chapters or skip markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub start_ms: u64,
    pub end_ms: u64,
}
