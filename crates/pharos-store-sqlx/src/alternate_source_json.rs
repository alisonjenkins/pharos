//! JSON column codec for `MediaProbe::alternate_sources` persistence.
//!
//! `AlternateMediaSource` already derives serde in `pharos-core`, so unlike
//! the track adapters this codec serialises it directly. Empty lists are
//! stored as NULL, matching `chapter_json`.

use pharos_core::AlternateMediaSource;

pub fn encode(sources: &[AlternateMediaSource]) -> Option<String> {
    if sources.is_empty() {
        return None;
    }
    match serde_json::to_string(sources) {
        Ok(s) => Some(s),
        Err(err) => {
            tracing::warn!(error = %err, count = sources.len(), "alternate sources not persisted: encode failed");
            None
        }
    }
}

pub fn decode(s: Option<&str>) -> Vec<AlternateMediaSource> {
    let Some(s) = s else {
        return Vec::new();
    };
    match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "alternate sources dropped: stored JSON failed to decode");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::path::PathBuf;

    #[test]
    fn round_trips_through_json() {
        let alts = vec![AlternateMediaSource {
            id: "directors-cut".into(),
            path: PathBuf::from("/m/Film - Director's Cut.mkv"),
            container: Some("matroska".into()),
            video_codec: Some("hevc".into()),
            audio_codec: Some("eac3".into()),
            bitrate_bps: Some(9_000_000),
            size_bytes: Some(12_345),
            duration_ms: Some(7_200_000),
            name: Some("Director's Cut".into()),
            video_3d_format: Some(pharos_core::Video3DFormat::HalfSideBySide),
        }];
        let s = encode(&alts).unwrap();
        assert_eq!(decode(Some(s.as_str())), alts);
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
