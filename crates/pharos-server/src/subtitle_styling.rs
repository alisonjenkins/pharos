//! Choosing what the subtitle styling job styles (spec 028 FR-012, FR-016).
//!
//! Two pure decisions, kept apart from the job so they are table-tested:
//! which subtitle track is the source, and which audio stream the subtitles
//! are timed to.

use crate::api::jellyfin::subtitles::is_image_subtitle_codec;
use pharos_core::AudioTrack;
use std::path::PathBuf;

/// A subtitle track the job could style: an embedded stream or a sidecar file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCandidate {
    /// The stream index the HTTP routes use; sidecars sit above
    /// `sidecar_base_index`.
    pub index: u32,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub hearing_impaired: bool,
    pub is_default: bool,
    pub forced: bool,
    /// The file, for a sidecar; `None` for an embedded stream.
    pub sidecar: Option<PathBuf>,
}

impl SubtitleCandidate {
    fn is_text(&self) -> bool {
        self.codec
            .as_deref()
            .is_some_and(|c| !c.is_empty() && !is_image_subtitle_codec(c))
    }

    fn is_ass(&self) -> bool {
        self.codec.as_deref().is_some_and(|c| {
            matches!(
                c.to_ascii_lowercase().as_str(),
                "ass" | "ssa" | "advanced substation alpha"
            )
        })
    }
}

fn is_english(language: Option<&str>) -> bool {
    let Some(l) = language else { return false };
    let l = l.trim().to_ascii_lowercase();
    matches!(l.as_str(), "eng" | "en" | "english")
        || l.starts_with("en-")
        || l.starts_with("en_")
        || l.starts_with("eng-")
}

fn is_untagged(language: Option<&str>) -> bool {
    language.map_or(true, |l| {
        let l = l.trim();
        l.is_empty() || l.eq_ignore_ascii_case("und")
    })
}

/// The source track (FR-016): a full English text track, never a forced one,
/// or an untagged one when it is the only text track. Among those, hearing
/// impaired first (its sound tags feed the director), then ASS/SSA over the
/// rest (italics and positions survive), then the default flag, then the
/// lowest index. Image codecs are never chosen.
pub fn choose_source(candidates: &[SubtitleCandidate]) -> Option<&SubtitleCandidate> {
    let text_tracks = candidates.iter().filter(|c| c.is_text()).count();
    candidates
        .iter()
        .filter(|c| c.is_text() && !c.forced)
        .filter(|c| {
            is_english(c.language.as_deref())
                || (text_tracks == 1 && is_untagged(c.language.as_deref()))
        })
        .min_by_key(|c| (!c.hearing_impaired, !c.is_ass(), !c.is_default, c.index))
}

/// The audio stream the subtitles are timed to (FR-012): the default track
/// when it is not English, else the first non-English track (an English dub
/// is not what the subtitles follow), else the default, else the first.
pub fn choose_audio(tracks: &[AudioTrack]) -> Option<u32> {
    let default = tracks.iter().find(|t| t.is_default);
    let non_english = |t: &&AudioTrack| !is_english(t.language.as_deref());
    default
        .filter(non_english)
        .or_else(|| tracks.iter().find(non_english))
        .or(default)
        .or_else(|| tracks.first())
        .map(|t| t.stream_index)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sub(index: u32, lang: Option<&str>, codec: &str) -> SubtitleCandidate {
        SubtitleCandidate {
            index,
            language: lang.map(str::to_owned),
            codec: Some(codec.to_owned()),
            hearing_impaired: false,
            is_default: false,
            forced: false,
            sidecar: None,
        }
    }

    fn chosen(c: &[SubtitleCandidate]) -> Option<u32> {
        choose_source(c).map(|c| c.index)
    }

    #[test]
    fn source_is_a_full_english_text_track() {
        assert_eq!(chosen(&[]), None);
        assert_eq!(
            chosen(&[sub(2, Some("jpn"), "ass"), sub(3, Some("eng"), "subrip")]),
            Some(3)
        );
        for lang in ["eng", "en", "English", "en-US"] {
            assert_eq!(chosen(&[sub(4, Some(lang), "subrip")]), Some(4), "{lang}");
        }
        assert_eq!(chosen(&[sub(2, Some("jpn"), "ass")]), None);
    }

    #[test]
    fn forced_and_image_tracks_are_never_the_source() {
        let mut forced = sub(2, Some("eng"), "ass");
        forced.forced = true;
        assert_eq!(chosen(&[forced]), None);
        assert_eq!(
            chosen(&[
                sub(2, Some("eng"), "hdmv_pgs_subtitle"),
                sub(3, Some("eng"), "dvd_subtitle")
            ]),
            None
        );
    }

    #[test]
    fn untagged_counts_only_as_the_single_text_track() {
        assert_eq!(chosen(&[sub(2, None, "subrip")]), Some(2));
        assert_eq!(chosen(&[sub(2, Some("und"), "subrip")]), Some(2));
        assert_eq!(
            chosen(&[sub(2, None, "subrip"), sub(3, Some("jpn"), "ass")]),
            None,
            "a second text track makes the untagged one a guess"
        );
        assert_eq!(
            chosen(&[sub(2, None, "subrip"), sub(3, Some("eng"), "subrip")]),
            Some(3)
        );
        // An image track beside it does not make it ambiguous.
        assert_eq!(
            chosen(&[
                sub(2, None, "subrip"),
                sub(3, Some("jpn"), "hdmv_pgs_subtitle")
            ]),
            Some(2)
        );
    }

    #[test]
    fn source_preference_is_sdh_then_ass_then_default_then_index() {
        let plain_ass = sub(5, Some("eng"), "ass");
        let mut sdh_srt = sub(7, Some("eng"), "subrip");
        sdh_srt.hearing_impaired = true;
        assert_eq!(chosen(&[plain_ass.clone(), sdh_srt.clone()]), Some(7));

        let srt = sub(2, Some("eng"), "subrip");
        assert_eq!(chosen(&[srt.clone(), plain_ass.clone()]), Some(5));

        let mut default_srt = sub(9, Some("eng"), "subrip");
        default_srt.is_default = true;
        assert_eq!(chosen(&[srt.clone(), default_srt]), Some(9));
        assert_eq!(chosen(&[sub(8, Some("eng"), "subrip"), srt]), Some(2));
    }

    fn audio(index: u32, lang: Option<&str>, default: bool) -> AudioTrack {
        AudioTrack {
            stream_index: index,
            language: lang.map(str::to_owned),
            is_default: default,
            ..AudioTrack::default()
        }
    }

    #[test]
    fn audio_follows_the_original_language_not_the_dub() {
        assert_eq!(choose_audio(&[]), None);
        // A Japanese default wins.
        assert_eq!(
            choose_audio(&[audio(1, Some("jpn"), true), audio(2, Some("eng"), false)]),
            Some(1)
        );
        // An English default yields to the first non-English track.
        assert_eq!(
            choose_audio(&[audio(1, Some("eng"), true), audio(2, Some("jpn"), false)]),
            Some(2)
        );
        // No default: the first non-English one.
        assert_eq!(
            choose_audio(&[audio(1, Some("eng"), false), audio(2, Some("jpn"), false)]),
            Some(2)
        );
        // Only English: the default, else the first.
        assert_eq!(
            choose_audio(&[audio(1, Some("eng"), false), audio(2, Some("en"), true)]),
            Some(2)
        );
        assert_eq!(
            choose_audio(&[audio(3, Some("eng"), false), audio(4, Some("eng"), false)]),
            Some(3)
        );
    }
}
