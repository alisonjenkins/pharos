//! JSON wire shape for `MediaProbe::attachments` persistence. Same pattern as
//! `subtitle_track_json` — the persistence layer owns the serde projection so
//! `pharos-core` stays serde-free at its trait surface.

use pharos_core::MediaAttachment;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MediaAttachmentJson {
    pub stream_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
}

impl From<&MediaAttachment> for MediaAttachmentJson {
    fn from(a: &MediaAttachment) -> Self {
        Self {
            stream_index: a.stream_index,
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            codec: a.codec.clone(),
        }
    }
}

impl From<MediaAttachmentJson> for MediaAttachment {
    fn from(j: MediaAttachmentJson) -> Self {
        Self {
            stream_index: j.stream_index,
            filename: j.filename,
            mime_type: j.mime_type,
            codec: j.codec,
        }
    }
}

/// Serialise attachments for the `attachments_json` column. Empty → `None`.
pub fn encode(atts: &[MediaAttachment]) -> Option<String> {
    if atts.is_empty() {
        return None;
    }
    let projected: Vec<MediaAttachmentJson> = atts.iter().map(Into::into).collect();
    serde_json::to_string(&projected).ok()
}

/// Parse the JSON column back into a Vec. Invalid / missing → empty.
pub fn decode(s: Option<&str>) -> Vec<MediaAttachment> {
    let Some(s) = s else { return Vec::new() };
    serde_json::from_str::<Vec<MediaAttachmentJson>>(s)
        .map(|v| v.into_iter().map(Into::into).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let atts = vec![MediaAttachment {
            stream_index: 7,
            filename: Some("Arial.ttf".into()),
            mime_type: Some("application/x-truetype-font".into()),
            codec: Some("ttf".into()),
        }];
        let s = encode(&atts).unwrap();
        assert_eq!(decode(Some(s.as_str())), atts);
    }

    #[test]
    fn empty_input_yields_none_and_missing_decodes_empty() {
        assert!(encode(&[]).is_none());
        assert!(decode(None).is_empty());
        assert!(decode(Some("not json")).is_empty());
    }
}
