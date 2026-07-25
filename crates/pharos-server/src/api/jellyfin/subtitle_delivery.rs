//! Per-format subtitle delivery decision from the client's DeviceProfile.
use crate::api::jellyfin::dto::is_text_subtitle_codec;
use crate::api::jellyfin::subtitles::is_image_subtitle_codec;
use pharos_jellyfin_api::device_profile::SubtitleProfileDto;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleDelivery {
    External,
    Burn,
}

fn format_matches(codec: &str, fmt: &str) -> bool {
    let c = codec.to_ascii_lowercase();
    let f = fmt.to_ascii_lowercase();
    c == f
        || (matches!(c.as_str(), "ass" | "ssa" | "advanced substation alpha")
            && matches!(f.as_str(), "ass" | "ssa"))
        || (c == "subrip" && matches!(f.as_str(), "subrip" | "srt"))
}

fn method_is_external(method: &str) -> bool {
    matches!(
        method.to_ascii_lowercase().as_str(),
        "external" | "embed" | "hls"
    )
}

fn is_ass_like(codec: &str) -> bool {
    matches!(codec, "ass" | "ssa" | "advanced substation alpha")
}

/// A plain-text delivery format pharos can produce from ANY plain-text
/// subtitle codec (the srt→WebVTT conversion every text sidecar already goes
/// through).
///
/// A `SubtitleProfile` names the format the client wants DELIVERED, not the
/// codec it expects in the source. jellyfin-web declares exactly one entry for
/// the whole plain-text family — `{Format:"vtt",Method:"External"}` — and
/// relies on the server to convert. Matching a source codec against that list
/// literally therefore found nothing for a `subrip` track and burned it: the
/// burn then ran a subtitles filter over every video segment, which is how one
/// SRT track took the whole browser playback path down.
fn is_convertible_text_format(fmt: &str) -> bool {
    matches!(fmt, "vtt" | "webvtt" | "srt" | "subrip")
}

pub fn decide_subtitle_delivery(
    codec: Option<&str>,
    client_profiles: &[SubtitleProfileDto],
) -> SubtitleDelivery {
    let codec = codec.unwrap_or("");
    let lower = codec.to_ascii_lowercase();
    let declares_external = |want: &dyn Fn(&str) -> bool| {
        client_profiles
            .iter()
            .any(|p| want(&p.format.to_ascii_lowercase()) && method_is_external(&p.method))
    };

    // An image sub converts to no text format at all, so only a client that
    // names this exact image format can render one itself (jellyfin-web
    // declares `pgssub` when its canvas PGS renderer is available).
    if is_image_subtitle_codec(&lower) {
        return if declares_external(&|f| format_matches(&lower, f)) {
            SubtitleDelivery::External
        } else {
            SubtitleDelivery::Burn
        };
    }
    if !is_text_subtitle_codec(Some(codec)) {
        return SubtitleDelivery::Burn; // unknown/other → safest is burn
    }
    if client_profiles.is_empty() {
        return SubtitleDelivery::External; // profile-less caller keeps the default
    }
    // ASS/SSA carry positioning, fonts and karaoke timing that a WebVTT
    // conversion drops (B104: the Android app rendered the converted result as
    // black bars), so they stay External only for a client that renders ASS
    // itself.
    if is_ass_like(&lower) {
        return if declares_external(&|f| format_matches(&lower, f)) {
            SubtitleDelivery::External
        } else {
            SubtitleDelivery::Burn
        };
    }
    if declares_external(&is_convertible_text_format) {
        SubtitleDelivery::External
    } else {
        SubtitleDelivery::Burn
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn prof(fmt: &str, method: &str) -> SubtitleProfileDto {
        SubtitleProfileDto {
            format: fmt.into(),
            method: method.into(),
            ..Default::default()
        }
    }

    #[test]
    fn web_declares_ass_external_gets_external() {
        let p = [prof("ass", "External"), prof("subrip", "External")];
        assert!(matches!(
            decide_subtitle_delivery(Some("ass"), &p),
            SubtitleDelivery::External
        ));
    }

    #[test]
    fn client_declaring_ass_encode_gets_burn() {
        let p = [prof("ass", "Encode"), prof("subrip", "External")];
        assert!(matches!(
            decide_subtitle_delivery(Some("ass"), &p),
            SubtitleDelivery::Burn
        ));
    }

    #[test]
    fn client_without_ass_profile_gets_burn() {
        let p = [prof("subrip", "External"), prof("vtt", "External")];
        assert!(matches!(
            decide_subtitle_delivery(Some("ass"), &p),
            SubtitleDelivery::Burn
        ));
    }

    #[test]
    fn image_codec_burns_when_the_client_cannot_render_it() {
        let p = [prof("ass", "External"), prof("vtt", "External")];
        assert!(matches!(
            decide_subtitle_delivery(Some("hdmv_pgs_subtitle"), &p),
            SubtitleDelivery::Burn
        ));
    }

    #[test]
    fn a_client_that_renders_pgs_itself_gets_it_externally() {
        let p = [prof("vtt", "External"), prof("pgssub", "External")];
        assert!(matches!(
            decide_subtitle_delivery(Some("pgssub"), &p),
            SubtitleDelivery::External
        ));
    }

    /// Exactly what jellyfin-web builds for a browser with SSA rendering and
    /// canvas PGS enabled. It names `vtt` for the entire plain-text family and
    /// expects the server to convert, so nothing here may burn a text track.
    fn jellyfin_web_profiles() -> [SubtitleProfileDto; 4] {
        [
            prof("vtt", "External"),
            prof("ass", "External"),
            prof("ssa", "External"),
            prof("pgssub", "External"),
        ]
    }

    /// The browser-playback outage: a `subrip` default track resolved to Burn
    /// because no profile literally said "subrip", so every video segment ran
    /// a subtitles filter and playback never started.
    #[test]
    fn a_vtt_profile_delivers_subrip_externally() {
        assert!(matches!(
            decide_subtitle_delivery(Some("subrip"), &jellyfin_web_profiles()),
            SubtitleDelivery::External
        ));
    }

    #[test]
    fn jellyfin_web_never_burns_a_plain_text_track() {
        for codec in [
            "subrip",
            "srt",
            "webvtt",
            "vtt",
            "mov_text",
            "text",
            "subviewer",
            "microdvd",
        ] {
            assert!(
                matches!(
                    decide_subtitle_delivery(Some(codec), &jellyfin_web_profiles()),
                    SubtitleDelivery::External
                ),
                "{codec} must be delivered, not burned"
            );
        }
    }

    /// A client that renders nothing but ASS still cannot be handed raw
    /// subrip — there is no plain-text format it accepts.
    #[test]
    fn a_client_declaring_only_ass_burns_subrip() {
        let p = [prof("ass", "External")];
        assert!(matches!(
            decide_subtitle_delivery(Some("subrip"), &p),
            SubtitleDelivery::Burn
        ));
    }

    #[test]
    fn empty_profiles_text_defaults_external() {
        assert!(matches!(
            decide_subtitle_delivery(Some("ass"), &[]),
            SubtitleDelivery::External
        ));
    }
}
