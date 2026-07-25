//! Why a video segment is — or is not — burning a subtitle into the picture.
//!
//! Burning is the single most expensive thing a segment transcode can do, and
//! until now the decision left no record: `burn=true` appeared only downstream
//! in the cache line, with no statement of what asked for it or why it was
//! honoured. A client that requested a burn it did not need was therefore
//! indistinguishable from one that genuinely required it, and the difference is
//! the difference between playback working and a 60-second encode queue.
//!
//! This is the decision, as data, so the log can name it.

use crate::api::jellyfin::subtitles::is_image_subtitle_codec;
use pharos_jellyfin_api::dto::is_text_subtitle_codec;

/// The verdict for one segment request's `SubtitleStreamIndex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnVerdict {
    /// The request carried no subtitle index.
    NotRequested,
    /// The index names no subtitle track on this media.
    UnknownTrack,
    /// The index names a track that is neither text nor a known image format —
    /// nothing can be done with it, so it is ignored rather than burned.
    UnsupportedCodec,
    /// An image subtitle (PGS / VOBSUB). No text rendition can be derived from
    /// a bitmap, so the only way to show it is to burn it.
    ImageSubtitle,
    /// A text subtitle this client cannot render itself, per the
    /// SubtitleProfiles it declared at PlaybackInfo.
    ClientCannotRenderText,
    /// A text subtitle the client renders itself. Burning it would be redundant
    /// AND slow, so the request is honoured by the external rendition instead.
    ClientRendersTextItself,
    /// A text subtitle requested with no live session to consult, so the
    /// client's SubtitleProfiles are unknown.
    TextNoSession,
}

impl BurnVerdict {
    /// Whether this verdict actually burns.
    pub fn burns(self) -> bool {
        matches!(
            self,
            Self::ImageSubtitle | Self::ClientCannotRenderText | Self::TextNoSession
        )
    }

    /// Stable metric label. Dashboards key on these, so the mapping lives here
    /// and is asserted distinct in a test rather than formatted at each site.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::UnknownTrack => "unknown_track",
            Self::UnsupportedCodec => "unsupported_codec",
            Self::ImageSubtitle => "image_subtitle",
            Self::ClientCannotRenderText => "client_cannot_render_text",
            Self::ClientRendersTextItself => "client_renders_text_itself",
            Self::TextNoSession => "text_no_session",
        }
    }
}

/// Decide whether a segment must burn the requested subtitle.
///
/// `burn_indices` is the set of ABSOLUTE stream indices this play session
/// established must be burned for THIS client — computed at PlaybackInfo from
/// the client's own SubtitleProfiles. `None` means no session was found and the
/// client's capabilities are therefore unknown.
///
/// A client asking for a burn does not make one necessary: jellyfin-web appends
/// `SubtitleStreamIndex` to every segment URL whenever the viewer has subtitles
/// switched on, including for the plain-text tracks it renders perfectly well
/// itself. Honouring that request literally burns a subtitle nobody needed, at
/// the cost of a filter graph on every segment.
pub fn decide_segment_burn(
    codec: Option<&str>,
    requested_abs_index: Option<u32>,
    burn_indices: Option<&std::collections::BTreeSet<u32>>,
) -> BurnVerdict {
    let Some(abs) = requested_abs_index else {
        return BurnVerdict::NotRequested;
    };
    let Some(codec) = codec else {
        return BurnVerdict::UnknownTrack;
    };
    if is_image_subtitle_codec(&codec.to_ascii_lowercase()) {
        return BurnVerdict::ImageSubtitle;
    }
    if !is_text_subtitle_codec(Some(codec)) {
        return BurnVerdict::UnsupportedCodec;
    }
    match burn_indices {
        // The session knows this client: burn only what it cannot render.
        Some(set) => {
            if set.contains(&abs) {
                BurnVerdict::ClientCannotRenderText
            } else {
                BurnVerdict::ClientRendersTextItself
            }
        }
        // No session — the client's profile is unknown. Burn, so a client that
        // genuinely cannot render the track is not left with no subtitle at
        // all; the session path above is what keeps this off the common case.
        None => BurnVerdict::TextNoSession,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::BTreeSet;

    /// The live bug: Firefox has every one of this file's 32 `subrip` tracks
    /// available to render itself, and jellyfin-web still puts
    /// `SubtitleStreamIndex=3` on every segment URL. Burning it cost a filter
    /// graph per segment and a 60-second encode queue.
    #[test]
    fn a_text_track_the_client_renders_is_not_burned() {
        let none_burn = BTreeSet::new();
        let v = decide_segment_burn(Some("subrip"), Some(3), Some(&none_burn));
        assert_eq!(v, BurnVerdict::ClientRendersTextItself);
        assert!(!v.burns(), "a renderable text sub must not burn");
    }

    #[test]
    fn a_text_track_the_client_cannot_render_still_burns() {
        let burn: BTreeSet<u32> = [3].into_iter().collect();
        let v = decide_segment_burn(Some("ass"), Some(3), Some(&burn));
        assert_eq!(v, BurnVerdict::ClientCannotRenderText);
        assert!(v.burns());
    }

    /// An image subtitle has no text rendition to fall back to, so it burns
    /// regardless of what the session says.
    #[test]
    fn an_image_track_always_burns() {
        let none_burn = BTreeSet::new();
        let v = decide_segment_burn(Some("hdmv_pgs_subtitle"), Some(3), Some(&none_burn));
        assert_eq!(v, BurnVerdict::ImageSubtitle);
        assert!(v.burns());
    }

    #[test]
    fn no_request_means_no_burn() {
        assert_eq!(
            decide_segment_burn(Some("subrip"), None, None),
            BurnVerdict::NotRequested
        );
        assert!(!BurnVerdict::NotRequested.burns());
    }

    #[test]
    fn an_unknown_track_or_codec_does_not_burn() {
        assert_eq!(
            decide_segment_burn(None, Some(99), None),
            BurnVerdict::UnknownTrack
        );
        assert_eq!(
            decide_segment_burn(Some("bin_data"), Some(3), None),
            BurnVerdict::UnsupportedCodec
        );
        assert!(!BurnVerdict::UnknownTrack.burns());
        assert!(!BurnVerdict::UnsupportedCodec.burns());
    }

    /// Without a session the client's capabilities are unknown, so the safe
    /// answer is to burn — better a burned subtitle than none at all.
    #[test]
    fn a_sessionless_text_request_burns() {
        let v = decide_segment_burn(Some("subrip"), Some(3), None);
        assert_eq!(v, BurnVerdict::TextNoSession);
        assert!(v.burns());
    }

    #[test]
    fn verdict_labels_are_distinct() {
        let all = [
            BurnVerdict::NotRequested,
            BurnVerdict::UnknownTrack,
            BurnVerdict::UnsupportedCodec,
            BurnVerdict::ImageSubtitle,
            BurnVerdict::ClientCannotRenderText,
            BurnVerdict::ClientRendersTextItself,
            BurnVerdict::TextNoSession,
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|v| v.label()).collect();
        assert_eq!(labels.len(), all.len());
    }
}
