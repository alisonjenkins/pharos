//! The dynamic range of a video stream, as Jellyfin's `VideoRangeType` names it.
//!
//! One enum so the probe, the negotiator's `VideoRangeType` condition, the
//! tonemap decision and the metric labels cannot drift apart as strings.

/// The ranges the probe can tell apart. Dolby Vision and HDR10+ are not among
/// them: the probe keeps no side data for either, so such a stream reports
/// whatever its base layer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoRangeType {
    Sdr,
    Hdr10,
    Hlg,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown video range type: {0:?}")]
pub struct ParseVideoRangeTypeError(pub String);

impl VideoRangeType {
    pub const ALL: [Self; 3] = [Self::Sdr, Self::Hdr10, Self::Hlg];

    /// The Jellyfin `VideoRangeType` token, which is also the stable metric
    /// label (`source_range` on `pharos_transcode_tonemap_total`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sdr => "SDR",
            Self::Hdr10 => "HDR10",
            Self::Hlg => "HLG",
        }
    }

    pub const fn is_hdr(self) -> bool {
        matches!(self, Self::Hdr10 | Self::Hlg)
    }
}

impl std::fmt::Display for VideoRangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for VideoRangeType {
    type Err = ParseVideoRangeTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|r| r.label() == s)
            .ok_or_else(|| ParseVideoRangeTypeError(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_the_jellyfin_tokens_and_distinct() {
        let labels: Vec<_> = VideoRangeType::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels, ["SDR", "HDR10", "HLG"]);
    }

    #[test]
    fn every_label_parses_back_and_unknown_tokens_do_not() {
        for r in VideoRangeType::ALL {
            assert_eq!(r.label().parse::<VideoRangeType>(), Ok(r));
        }
        assert!("DOVI".parse::<VideoRangeType>().is_err());
        assert!("hdr10".parse::<VideoRangeType>().is_err());
    }

    #[test]
    fn only_hdr10_and_hlg_are_hdr() {
        let hdr: Vec<_> = VideoRangeType::ALL
            .into_iter()
            .filter(|r| r.is_hdr())
            .collect();
        assert_eq!(hdr, [VideoRangeType::Hdr10, VideoRangeType::Hlg]);
    }
}
