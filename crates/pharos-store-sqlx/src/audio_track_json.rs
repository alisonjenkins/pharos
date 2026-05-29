//! P16 — JSON wire shape for `MediaProbe::audio_tracks` persistence.
//!
//! Mirrors the `subtitle_track_json` module so the new column follows
//! the same encode/decode + back-compat semantics: empty input maps
//! to None (no `[]` bloat in rows), malformed JSON decodes to an
//! empty Vec so a future schema bump never bricks an older row.

use pharos_core::AudioTrack;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AudioTrackJson {
    pub stream_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
    /// P37 — track-level ReplayGain in centidecibels (gain × 100).
    /// Rows persisted before this field landed decode to None via the
    /// serde default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaygain_track_centidb: Option<i16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaygain_album_centidb: Option<i16>,
}

impl From<&AudioTrack> for AudioTrackJson {
    fn from(t: &AudioTrack) -> Self {
        Self {
            stream_index: t.stream_index,
            codec: t.codec.clone(),
            channels: t.channels,
            sample_rate: t.sample_rate,
            language: t.language.clone(),
            title: t.title.clone(),
            is_default: t.is_default,
            replaygain_track_centidb: t.replaygain_track_centidb,
            replaygain_album_centidb: t.replaygain_album_centidb,
        }
    }
}

impl From<AudioTrackJson> for AudioTrack {
    fn from(j: AudioTrackJson) -> Self {
        Self {
            stream_index: j.stream_index,
            codec: j.codec,
            channels: j.channels,
            sample_rate: j.sample_rate,
            language: j.language,
            title: j.title,
            is_default: j.is_default,
            replaygain_track_centidb: j.replaygain_track_centidb,
            replaygain_album_centidb: j.replaygain_album_centidb,
        }
    }
}

pub fn encode(tracks: &[AudioTrack]) -> Option<String> {
    if tracks.is_empty() {
        return None;
    }
    let projected: Vec<AudioTrackJson> = tracks.iter().map(Into::into).collect();
    serde_json::to_string(&projected).ok()
}

pub fn decode(s: Option<&str>) -> Vec<AudioTrack> {
    let Some(s) = s else { return Vec::new() };
    serde_json::from_str::<Vec<AudioTrackJson>>(s)
        .map(|v| v.into_iter().map(Into::into).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn round_trips_through_json() {
        let tracks = vec![
            AudioTrack {
                stream_index: 1,
                codec: Some("aac".into()),
                channels: Some(2),
                sample_rate: Some(48_000),
                language: Some("eng".into()),
                title: Some("English".into()),
                is_default: true,
                replaygain_track_centidb: Some(-734),
                replaygain_album_centidb: Some(-682),
            },
            AudioTrack {
                stream_index: 2,
                codec: Some("ac3".into()),
                channels: Some(6),
                sample_rate: Some(48_000),
                language: Some("jpn".into()),
                title: None,
                is_default: false,
                replaygain_track_centidb: None,
                replaygain_album_centidb: None,
            },
        ];
        let s = encode(&tracks).unwrap();
        let back = decode(Some(s.as_str()));
        assert_eq!(back, tracks);
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

    #[test]
    fn legacy_rows_without_replaygain_decode_to_none() {
        // P37 — rows persisted before the ReplayGain fields landed
        // must still decode and produce `None` track/album gain.
        let legacy = r#"[{"stream_index":1,"codec":"aac","channels":2,"sample_rate":48000,"is_default":true}]"#;
        let back = decode(Some(legacy));
        assert_eq!(back.len(), 1);
        assert!(back[0].replaygain_track_centidb.is_none());
        assert!(back[0].replaygain_album_centidb.is_none());
    }
}
