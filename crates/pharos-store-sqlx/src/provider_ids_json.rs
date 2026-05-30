//! JSON wire shape for `MediaMetadata::provider_ids` persistence (LIB-C9).
//!
//! Mirrors `chapter_json` / `subtitle_track_json` — the `provider_ids`
//! column stores a JSON object string (`{"tmdb":"…","imdb":"…"}`).
//! pharos-core's [`ProviderIds`] already derives serde, so we project it
//! straight through; an all-`None` value encodes to `None` (NULL column)
//! so the empty case round-trips cleanly.

use pharos_core::ProviderIds;

/// Encode to a JSON object string, or `None` when no provider id is set
/// (stored as a NULL column).
pub fn encode(ids: &ProviderIds) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    serde_json::to_string(ids).ok()
}

/// Decode a stored JSON object string back into [`ProviderIds`]. `None`
/// or malformed JSON yields `ProviderIds::default()` (all-`None`).
pub fn decode(s: Option<&str>) -> ProviderIds {
    let Some(s) = s else {
        return ProviderIds::default();
    };
    serde_json::from_str::<ProviderIds>(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn round_trips_through_json() {
        let ids = ProviderIds {
            tmdb: Some("603".into()),
            imdb: Some("tt0133093".into()),
            ..Default::default()
        };
        let s = encode(&ids).unwrap();
        let back = decode(Some(s.as_str()));
        assert_eq!(back, ids);
    }

    #[test]
    fn empty_encodes_to_none_and_decodes_to_default() {
        assert!(encode(&ProviderIds::default()).is_none());
        assert_eq!(decode(None), ProviderIds::default());
    }

    #[test]
    fn malformed_json_decodes_to_default() {
        assert_eq!(decode(Some("not json")), ProviderIds::default());
    }
}
