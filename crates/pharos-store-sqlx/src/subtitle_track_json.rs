//! JSON wire shape for `MediaProbe::subtitle_tracks` persistence.
//!
//! pharos-core is deliberately serde-free (its trait surface is IO-
//! agnostic). The persistence layer owns the JSON projection so the
//! domain crate stays slim.

use pharos_core::SubtitleTrack;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SubtitleTrackJson {
    pub stream_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_forced: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_hearing_impaired: bool,
}

impl From<&SubtitleTrack> for SubtitleTrackJson {
    fn from(t: &SubtitleTrack) -> Self {
        Self {
            stream_index: t.stream_index,
            language: t.language.clone(),
            codec: t.codec.clone(),
            title: t.title.clone(),
            is_default: t.is_default,
            is_forced: t.is_forced,
            is_hearing_impaired: t.is_hearing_impaired,
        }
    }
}

impl From<SubtitleTrackJson> for SubtitleTrack {
    fn from(j: SubtitleTrackJson) -> Self {
        Self {
            stream_index: j.stream_index,
            language: j.language,
            codec: j.codec,
            title: j.title,
            is_default: j.is_default,
            is_forced: j.is_forced,
            is_hearing_impaired: j.is_hearing_impaired,
        }
    }
}

/// Serialise the tracks to JSON for the `subtitle_tracks_json` column.
/// Empty input → `None` (don't bloat rows with `[]`).
pub fn encode(tracks: &[SubtitleTrack]) -> Option<String> {
    if tracks.is_empty() {
        return None;
    }
    let projected: Vec<SubtitleTrackJson> = tracks.iter().map(Into::into).collect();
    serde_json::to_string(&projected).ok()
}

/// Parse the JSON column back into a Vec. Invalid / missing → empty.
pub fn decode(s: Option<&str>) -> Vec<SubtitleTrack> {
    let Some(s) = s else { return Vec::new() };
    serde_json::from_str::<Vec<SubtitleTrackJson>>(s)
        .map(|v| v.into_iter().map(Into::into).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn round_trips_through_json() {
        let tracks = vec![SubtitleTrack {
            stream_index: 2,
            language: Some("eng".into()),
            codec: Some("webvtt".into()),
            title: Some("English".into()),
            is_default: true,
            is_forced: false,
            is_hearing_impaired: true,
        }];
        let s = encode(&tracks).unwrap();
        let back = decode(Some(s.as_str()));
        assert_eq!(back, tracks);
    }

    #[test]
    fn legacy_rows_without_hearing_impaired_decode_to_false() {
        // P35 — rows persisted before the field was added must still
        // decode. The skip-serializing-default ensures new writes
        // don't bloat existing pre-SDH rows either.
        let legacy = r#"[{"stream_index":2,"language":"eng","is_default":true}]"#;
        let back = decode(Some(legacy));
        assert_eq!(back.len(), 1);
        assert!(!back[0].is_hearing_impaired);
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
