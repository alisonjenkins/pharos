//! Stereoscopic 3D packing of a video file.
//!
//! The wire names are Jellyfin's `Video3DFormat` enum, which jellyfin-web and
//! the SDK clients read off `BaseItemDto` and `MediaSourceInfo`. The short
//! tags are the filename flags (`Film - 3D.HSBS.mkv`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Video3DFormat {
    HalfSideBySide,
    FullSideBySide,
    HalfTopAndBottom,
    FullTopAndBottom,
    // Editions stored before the rename hold "Mvc"; dropping it would
    // fail the whole alternate_sources_json decode.
    #[serde(rename = "MVC", alias = "Mvc")]
    Mvc,
}

impl Video3DFormat {
    /// Name Jellyfin serialises for this format.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::HalfSideBySide => "HalfSideBySide",
            Self::FullSideBySide => "FullSideBySide",
            Self::HalfTopAndBottom => "HalfTopAndBottom",
            Self::FullTopAndBottom => "FullTopAndBottom",
            Self::Mvc => "MVC",
        }
    }

    /// Short upper-case tag used in filenames and edition labels.
    pub const fn short_tag(self) -> &'static str {
        match self {
            Self::HalfSideBySide => "HSBS",
            Self::FullSideBySide => "FSBS",
            Self::HalfTopAndBottom => "HTAB",
            Self::FullTopAndBottom => "FTAB",
            Self::Mvc => "MVC",
        }
    }

    /// Parse a filename tag, case-insensitively. Unknown tags are `None`.
    pub fn from_short_tag(tag: &str) -> Option<Self> {
        match tag.to_ascii_uppercase().as_str() {
            "HSBS" => Some(Self::HalfSideBySide),
            "FSBS" => Some(Self::FullSideBySide),
            "HTAB" => Some(Self::HalfTopAndBottom),
            "FTAB" => Some(Self::FullTopAndBottom),
            "MVC" => Some(Self::Mvc),
            _ => None,
        }
    }

    /// Parse the persisted / wire name written by [`Self::wire_name`].
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "HalfSideBySide" => Some(Self::HalfSideBySide),
            "FullSideBySide" => Some(Self::FullSideBySide),
            "HalfTopAndBottom" => Some(Self::HalfTopAndBottom),
            "FullTopAndBottom" => Some(Self::FullTopAndBottom),
            "MVC" => Some(Self::Mvc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown 3D video format: {0:?}")]
pub struct ParseVideo3DFormatError(pub String);

impl Video3DFormat {
    pub const ALL: [Self; 5] = [
        Self::HalfSideBySide,
        Self::FullSideBySide,
        Self::FullTopAndBottom,
        Self::HalfTopAndBottom,
        Self::Mvc,
    ];

    /// Returns `true` for frame-packed SBS and TAB layouts requiring anamorphic
    /// or full-frame display/downmix, and `false` for MVC.
    pub const fn is_packed_sbs_or_tab(&self) -> bool {
        match self {
            Self::HalfSideBySide
            | Self::FullSideBySide
            | Self::FullTopAndBottom
            | Self::HalfTopAndBottom => true,
            Self::Mvc => false,
        }
    }

    /// Pure classifier resolving stereoscopic layout and metric label from prober signals.
    ///
    /// Evaluates in order:
    /// 1. H.264 MVC codec or profile (`"mvc"`, `"h264_mvc"`, `"Stereo High"`, `"Multiview High"`,
    ///    `"128"`, `"118"`, or `has_mvc_substream`).
    /// 2. Container tag `stereo_mode` (Matroska / WebM).
    /// 3. Coded packet side data `AV_PKT_DATA_STEREO3D` (MP4/MOV).
    ///
    /// Returns `(format, metric_label)` where `metric_label` is one of `"probe_mvc"`,
    /// `"probe_tag"`, `"probe_sidedata"`, or `"anaglyph"`.
    pub fn from_probe_signals(
        stereo_mode_tag: Option<&str>,
        side_data_type: Option<&str>,
        video_codec: Option<&str>,
        video_profile: Option<&str>,
        has_mvc_substream: bool,
        width: Option<u32>,
        height: Option<u32>,
    ) -> (Option<Self>, Option<&'static str>) {
        let is_mvc = has_mvc_substream
            || video_codec.is_some_and(|c| {
                let trimmed = c.trim();
                trimmed.eq_ignore_ascii_case("mvc") || trimmed.eq_ignore_ascii_case("h264_mvc")
            })
            || video_profile.is_some_and(|p| {
                let trimmed = p.trim();
                trimmed.eq_ignore_ascii_case("stereo high")
                    || trimmed.eq_ignore_ascii_case("multiview high")
                    || trimmed == "128"
                    || trimmed == "118"
            });

        if is_mvc {
            return (Some(Self::Mvc), Some(Detect3DSource::ProbeMvc.label()));
        }

        fn classify_sbs(width: Option<u32>, height: Option<u32>) -> Video3DFormat {
            match (width, height) {
                (Some(w), Some(h)) if h > 0 && w >= h.saturating_mul(3) => {
                    Video3DFormat::FullSideBySide
                }
                _ => Video3DFormat::HalfSideBySide,
            }
        }

        fn classify_tab(width: Option<u32>, height: Option<u32>) -> Video3DFormat {
            match (width, height) {
                (Some(w), Some(h)) if h > 0 && w <= h => Video3DFormat::FullTopAndBottom,
                _ => Video3DFormat::HalfTopAndBottom,
            }
        }

        if let Some(tag) = stereo_mode_tag {
            let tag = tag.trim().to_ascii_lowercase();
            match tag.as_str() {
                "left_right" | "right_left" | "1" | "2" | "11" | "side_by_side"
                | "side by side" | "sidebyside" => {
                    return (
                        Some(classify_sbs(width, height)),
                        Some(Detect3DSource::ProbeTag.label()),
                    );
                }
                "bottom_top" | "top_bottom" | "3" | "4" | "top_and_bottom" | "top and bottom"
                | "topbottom" => {
                    return (
                        Some(classify_tab(width, height)),
                        Some(Detect3DSource::ProbeTag.label()),
                    );
                }
                "block_lr" | "block_rl" | "frame alternate" | "frame_alternate"
                | "framesequence" | "13" | "14" => {
                    return (
                        Some(Video3DFormat::Mvc),
                        Some(Detect3DSource::ProbeTag.label()),
                    );
                }
                _ if tag.starts_with("anaglyph") => {
                    return (None, Some("anaglyph"));
                }
                "mono" | "0" => {
                    // Explicit 2D container tag; fall through to side_data_type check
                }
                _ => {
                    // Unrecognized tag; fall through to side_data_type check
                }
            }
        }

        if let Some(side_data) = side_data_type {
            let sd = side_data.trim().to_ascii_lowercase();
            if sd == "side by side"
                || sd == "side_by_side"
                || sd == "sidebyside"
                || sd == "side by side (quincunx)"
                || sd.contains("side by side")
            {
                return (
                    Some(classify_sbs(width, height)),
                    Some(Detect3DSource::ProbeSideData.label()),
                );
            } else if sd == "top and bottom"
                || sd == "top_and_bottom"
                || sd == "topbottom"
                || sd.contains("top and bottom")
            {
                return (
                    Some(classify_tab(width, height)),
                    Some(Detect3DSource::ProbeSideData.label()),
                );
            } else if sd == "frame alternate" || sd == "frame_alternate" || sd == "framesequence" {
                return (
                    Some(Video3DFormat::Mvc),
                    Some(Detect3DSource::ProbeSideData.label()),
                );
            } else if sd.contains("anaglyph") {
                return (None, Some("anaglyph"));
            }
        }

        (None, None)
    }
}

impl std::fmt::Display for Video3DFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

impl std::str::FromStr for Video3DFormat {
    type Err = ParseVideo3DFormatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        match trimmed {
            "HalfSideBySide" => Ok(Self::HalfSideBySide),
            "FullSideBySide" => Ok(Self::FullSideBySide),
            "FullTopAndBottom" => Ok(Self::FullTopAndBottom),
            "HalfTopAndBottom" => Ok(Self::HalfTopAndBottom),
            "MVC" => Ok(Self::Mvc),
            _ if trimmed.eq_ignore_ascii_case("mvc") => Ok(Self::Mvc),
            _ if trimmed.eq_ignore_ascii_case("halfsidebyside") => Ok(Self::HalfSideBySide),
            _ if trimmed.eq_ignore_ascii_case("fullsidebyside") => Ok(Self::FullSideBySide),
            _ if trimmed.eq_ignore_ascii_case("fulltopandbottom") => Ok(Self::FullTopAndBottom),
            _ if trimmed.eq_ignore_ascii_case("halftopandbottom") => Ok(Self::HalfTopAndBottom),
            _ => Err(ParseVideo3DFormatError(s.to_string())),
        }
    }
}

/// ODD detection source label for 3D media format identification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Detect3DSource {
    ProbeTag,
    ProbeSideData,
    ProbeMvc,
    Filename,
}

impl Detect3DSource {
    pub const ALL: [Self; 4] = [
        Self::ProbeTag,
        Self::ProbeSideData,
        Self::ProbeMvc,
        Self::Filename,
    ];

    pub const fn label(&self) -> &'static str {
        match self {
            Self::ProbeTag => "probe_tag",
            Self::ProbeSideData => "probe_sidedata",
            Self::ProbeMvc => "probe_mvc",
            Self::Filename => "filename",
        }
    }

    pub const fn as_str(&self) -> &'static str {
        self.label()
    }
}

impl std::fmt::Display for Detect3DSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Video3DFormat; 5] = [
        Video3DFormat::HalfSideBySide,
        Video3DFormat::FullSideBySide,
        Video3DFormat::HalfTopAndBottom,
        Video3DFormat::FullTopAndBottom,
        Video3DFormat::Mvc,
    ];

    #[test]
    fn wire_name_round_trips() {
        for f in ALL {
            assert_eq!(Video3DFormat::from_wire_name(f.wire_name()), Some(f));
        }
    }

    #[test]
    fn short_tag_round_trips_case_insensitively() {
        for f in ALL {
            assert_eq!(Video3DFormat::from_short_tag(f.short_tag()), Some(f));
            assert_eq!(
                Video3DFormat::from_short_tag(&f.short_tag().to_lowercase()),
                Some(f)
            );
        }
    }

    #[test]
    fn unknown_names_are_none() {
        assert_eq!(Video3DFormat::from_short_tag("SBS"), None);
        assert_eq!(Video3DFormat::from_wire_name("mvc"), None);
    }

    #[test]
    fn serde_reads_the_pre_rename_mvc_spelling() {
        use serde::de::value::{Error, StrDeserializer};
        let f = Video3DFormat::deserialize(StrDeserializer::<Error>::new("Mvc"));
        assert_eq!(f.ok(), Some(Video3DFormat::Mvc));
    }
}
