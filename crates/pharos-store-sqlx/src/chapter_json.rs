//! JSON wire shape for `MediaProbe::chapters` persistence.
//!
//! Mirrors `subtitle_track_json` — pharos-core stays serde-free; this
//! adapter owns the JSON projection.

use pharos_core::MediaChapter;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MediaChapterJson {
    pub start_ms: u64,
    pub end_ms: u64,
    pub title: String,
}

impl From<&MediaChapter> for MediaChapterJson {
    fn from(c: &MediaChapter) -> Self {
        Self {
            start_ms: c.start_ms,
            end_ms: c.end_ms,
            title: c.title.clone(),
        }
    }
}

impl From<MediaChapterJson> for MediaChapter {
    fn from(j: MediaChapterJson) -> Self {
        Self {
            start_ms: j.start_ms,
            end_ms: j.end_ms,
            title: j.title,
        }
    }
}

pub fn encode(chapters: &[MediaChapter]) -> Option<String> {
    if chapters.is_empty() {
        return None;
    }
    let projected: Vec<MediaChapterJson> = chapters.iter().map(Into::into).collect();
    serde_json::to_string(&projected).ok()
}

pub fn decode(s: Option<&str>) -> Vec<MediaChapter> {
    let Some(s) = s else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<MediaChapterJson>>(s)
        .map(|v| v.into_iter().map(Into::into).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn round_trips_through_json() {
        let chs = vec![
            MediaChapter {
                start_ms: 0,
                end_ms: 300_000,
                title: "Opening".into(),
            },
            MediaChapter {
                start_ms: 300_000,
                end_ms: 1_800_000,
                title: "Chapter 2".into(),
            },
        ];
        let s = encode(&chs).unwrap();
        let back = decode(Some(s.as_str()));
        assert_eq!(back, chs);
    }

    #[test]
    fn empty_input_yields_none() {
        assert!(encode(&[]).is_none());
        assert!(decode(None).is_empty());
    }

    #[test]
    fn malformed_json_decodes_to_empty() {
        assert!(decode(Some("not json")).is_empty());
    }
}
