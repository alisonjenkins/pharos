//! Wire types of the H.264 copy surface (spec 041, ADR-0123): what the worker
//! reports about a source it was asked to index, and what it wrote for one
//! copied segment. The libav side lives in `libav::copy_segment`; the boxes
//! are written by [`crate::copy_fmp4`].

use crate::copy_fmp4::CopyTrack;
use pharos_core::copy_plan::{CopyPlan, PlanRefusal};
use serde::{Deserialize, Serialize};

/// Plan segments read and written in full when a source is indexed, so an
/// open GOP or an index that does not match the packets is found before any
/// player asks for a segment.
pub const VALIDATED_SEGMENTS: usize = 3;

/// A source the copy surface can serve, in its own video time base with the
/// container's start time subtracted (the timeline every other surface uses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySource {
    pub track: CopyTrack,
    pub plan: CopyPlan,
    /// Container start time in the track's timescale; added back to every
    /// plan time before it is used to seek the source.
    pub origin: i64,
}

/// Why a source cannot be copied. Each carries the value that decided it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum CopyRefusal {
    #[error("container {0} is not matroska")]
    Container(String),
    #[error("no video stream")]
    NoVideo,
    #[error("video codec {0} is not h264")]
    Codec(String),
    #[error("codec private data is not an avcC record (first byte {first:?}, {len} bytes)")]
    NotAvcc { first: Option<u8>, len: usize },
    #[error("h264 profile_idc {0} is not baseline, main or high")]
    Profile(u8),
    #[error("pixel format {0} is not yuv420p")]
    PixelFormat(String),
    #[error("field order {0} is not progressive")]
    Interlaced(String),
    #[error("video time base {num}/{den} is not 1/n")]
    TimeBase { num: i32, den: i32 },
    #[error("no keyframes in the seek index")]
    NoIndex,
    #[error("unknown duration")]
    NoDuration,
    #[error(
        "first video packet (pts {pts}, keyframe {keyframe}) is not the first indexed keyframe at {first_keyframe}"
    )]
    LeadingPackets {
        pts: i64,
        keyframe: bool,
        first_keyframe: i64,
    },
    #[error("plan: {0}")]
    Plan(PlanRefusal),
    #[error("segment {seg}: {detail}")]
    Segment { seg: usize, detail: String },
}

impl CopyRefusal {
    /// Stable metric label, one per variant.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Container(_) => "container",
            Self::NoVideo => "no_video",
            Self::Codec(_) => "codec",
            Self::NotAvcc { .. } => "not_avcc",
            Self::Profile(_) => "profile",
            Self::PixelFormat(_) => "pixel_format",
            Self::Interlaced(_) => "interlaced",
            Self::TimeBase { .. } => "time_base",
            Self::NoIndex => "no_index",
            Self::NoDuration => "no_duration",
            Self::LeadingPackets { .. } => "leading_packets",
            Self::Plan(_) => "plan",
            Self::Segment { .. } => "segment",
        }
    }
}

/// The answer to indexing a source: copyable with its plan, or why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyIndexVerdict {
    Copyable(Box<CopySource>),
    Refused(CopyRefusal),
}

/// What one copied segment held, for the serve log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySegmentReport {
    pub packets: u32,
    pub bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_labels_are_distinct() {
        let all = [
            CopyRefusal::Container(String::new()),
            CopyRefusal::NoVideo,
            CopyRefusal::Codec(String::new()),
            CopyRefusal::NotAvcc {
                first: None,
                len: 0,
            },
            CopyRefusal::Profile(0),
            CopyRefusal::PixelFormat(String::new()),
            CopyRefusal::Interlaced(String::new()),
            CopyRefusal::TimeBase { num: 0, den: 0 },
            CopyRefusal::NoIndex,
            CopyRefusal::NoDuration,
            CopyRefusal::LeadingPackets {
                pts: 0,
                keyframe: false,
                first_keyframe: 0,
            },
            CopyRefusal::Plan(PlanRefusal::NoKeyframes),
            CopyRefusal::Segment {
                seg: 0,
                detail: String::new(),
            },
        ];
        let mut labels: Vec<_> = all.iter().map(CopyRefusal::label).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
    }
}
