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

pub fn decide_subtitle_delivery(
    codec: Option<&str>,
    client_profiles: &[SubtitleProfileDto],
) -> SubtitleDelivery {
    let codec = codec.unwrap_or("");
    if is_image_subtitle_codec(&codec.to_ascii_lowercase()) {
        return SubtitleDelivery::Burn;
    }
    if !is_text_subtitle_codec(Some(codec)) {
        return SubtitleDelivery::Burn; // unknown/other → safest is burn
    }
    if client_profiles.is_empty() {
        return SubtitleDelivery::External; // profile-less caller keeps the default
    }
    let has_external = client_profiles
        .iter()
        .any(|p| format_matches(codec, &p.format) && method_is_external(&p.method));
    if has_external {
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
    fn image_codec_always_burns() {
        let p = [prof("ass", "External")];
        assert!(matches!(
            decide_subtitle_delivery(Some("hdmv_pgs_subtitle"), &p),
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
