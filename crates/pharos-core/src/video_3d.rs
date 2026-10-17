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
}
