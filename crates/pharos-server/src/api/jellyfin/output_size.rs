//! Client-driven output downscaling for video transcodes (spec 035, ADR-0092).
//!
//! Jellyfin clients bound the frame with `MaxWidth` / `MaxHeight` (and, less
//! often, `Width` / `Height`) on the master playlist and stream URLs, and
//! bound the bitrate with `MaxStreamingBitrate`, which PlaybackInfo bakes into
//! the transcode URL as `VideoBitrate`. This module turns those into the
//! [`OutputSize`] the transcoder scales to, and why.

use pharos_core::{MediaProbe, Video3DFormat};
use pharos_transcode::options::StereoOutput;
use pharos_transcode::output_size::{plan_delivered, ScaleReason, SizeLimits};
use pharos_transcode::OutputSize;

/// Query keys that bound the output frame. `Max*` is the contract; the bare
/// `Width` / `Height` some clients send are read as the same kind of bound (a
/// transcode never upscales, so they cannot ask for more than the source).
const WIDTH_KEYS: [&str; 2] = ["MaxWidth", "Width"];
const HEIGHT_KEYS: [&str; 2] = ["MaxHeight", "Height"];

/// The query keys [`qs_size_limits`] reads, for the code that forwards them
/// master -> variant -> segment.
pub(crate) fn is_size_key(key: &str) -> bool {
    WIDTH_KEYS
        .iter()
        .chain(HEIGHT_KEYS.iter())
        .any(|k| key.eq_ignore_ascii_case(k))
}

fn qs_edge(qs: &str, keys: &[&str]) -> Option<u32> {
    // The tightest of the keys that parse: `MaxWidth=1920&Width=1280` is 1280.
    qs.split('&')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| keys.iter().any(|w| k.eq_ignore_ascii_case(w)))
        .filter_map(|(_, v)| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .min()
}

/// The client's bounding box from a request query string. Unparseable, zero
/// and absurd values read as absent.
pub(crate) fn qs_size_limits(qs: &str) -> SizeLimits {
    SizeLimits::new(qs_edge(qs, &WIDTH_KEYS), qs_edge(qs, &HEIGHT_KEYS))
}

/// Decide the output size for a re-encode of `probe`, and why.
///
/// Pure: the decision is counted where a scaled transcode actually starts
/// (`pharos_cache::hls_cache`, and the progressive stream), not here, because
/// this runs for cache hits, init segments and prewarm too.
///
/// `bitrate_capped` is true only when the video bitrate was set by the client
/// (a negotiated cap, a ladder rung): a low bitrate that merely follows a
/// low-bitrate source is not a reason to shrink the picture.
///
/// A packed 3D source downmixed to 2D is bounded on the frame the client
/// receives (one eye), not on the packed frame the scale runs on.
pub(crate) fn plan_output_size(
    probe: &MediaProbe,
    stereo_output: StereoOutput,
    limits: SizeLimits,
    video_bitrate_bps: Option<u64>,
    bitrate_capped: bool,
) -> Option<(OutputSize, ScaleReason)> {
    let (w, h) = (probe.width?, probe.height?);
    // A packed 3D frame keeps each half even, whether it is preserved or a
    // 2D downmix crops one half out of it.
    let align = if probe.video_3d_format.is_some() {
        4
    } else {
        2
    };
    let (dw, dh) = match (stereo_output, probe.video_3d_format) {
        (StereoOutput::Mono2D, Some(Video3DFormat::FullSideBySide)) => (w / 2, h),
        (StereoOutput::Mono2D, Some(Video3DFormat::FullTopAndBottom)) => (w, h / 2),
        _ => (w, h),
    };
    let bitrate = video_bitrate_bps.filter(|_| bitrate_capped);
    let planned = plan_delivered(w, h, dw, dh, limits, bitrate, align)?;
    tracing::debug!(
        source.width = w,
        source.height = h,
        output.width = planned.0.width(),
        output.height = planned.0.height(),
        reason = planned.1.label(),
        video_bitrate_bps,
        "transcode output downscaled"
    );
    Some(planned)
}

/// Count one scaled transcode start. Called by the progressive stream; the HLS
/// cache counts its own.
pub(crate) fn count_scaled(reason: ScaleReason) {
    metrics::counter!(
        "pharos_transcode_output_scaled_total",
        "reason" => reason.label(),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_transcode::output_size::plan;

    fn probe(w: u32, h: u32) -> MediaProbe {
        MediaProbe {
            width: Some(w),
            height: Some(h),
            ..Default::default()
        }
    }

    #[test]
    fn size_keys_parse_case_insensitively_and_the_tightest_wins() {
        let l = qs_size_limits("maxwidth=1920&MaxHeight=1080&Width=1280");
        let (s, _) = plan(3840, 2160, l, None, 2).unwrap();
        assert_eq!((s.width(), s.height()), (1280, 720));
    }

    #[test]
    fn junk_and_zero_sizes_are_no_bound() {
        assert!(qs_size_limits("MaxWidth=abc&MaxHeight=0&Width=-5").is_empty());
        assert!(qs_size_limits("").is_empty());
    }

    #[test]
    fn the_bounds_baked_into_a_transcoding_url_read_back_the_same() {
        let l = qs_size_limits("MaxWidth=1280&MaxHeight=720");
        assert_eq!(qs_size_limits(&l.query_suffix()), l);
        assert!(is_size_key("MaxWidth") && is_size_key("MaxHeight"));
    }

    #[test]
    fn size_keys_are_the_ones_forwarded() {
        for k in ["MaxWidth", "maxheight", "Width", "HEIGHT"] {
            assert!(is_size_key(k), "{k}");
        }
        assert!(!is_size_key("VideoBitrate"));
    }

    const FLAT: StereoOutput = StereoOutput::Preserve;

    fn probe3d(w: u32, h: u32, f: Video3DFormat) -> MediaProbe {
        MediaProbe {
            video_3d_format: Some(f),
            ..probe(w, h)
        }
    }

    #[test]
    fn a_bound_scales_a_4k_source_for_max_size() {
        let (size, reason) = plan_output_size(
            &probe(3840, 2160),
            FLAT,
            qs_size_limits("MaxWidth=1920"),
            Some(8_000_000),
            true,
        )
        .unwrap();
        assert_eq!((size.width(), size.height()), (1920, 1080));
        assert_eq!(reason, ScaleReason::MaxSize);
    }

    #[test]
    fn a_client_bitrate_cap_picks_a_lower_rung_for_bitrate() {
        let (size, reason) = plan_output_size(
            &probe(3840, 2160),
            FLAT,
            SizeLimits::default(),
            Some(2_000_000),
            true,
        )
        .unwrap();
        assert_eq!(size.height(), 1080);
        assert_eq!(reason, ScaleReason::Bitrate);
    }

    #[test]
    fn a_low_bitrate_that_follows_the_source_is_not_a_reason_to_shrink() {
        assert!(plan_output_size(
            &probe(3840, 2160),
            FLAT,
            SizeLimits::default(),
            Some(2_000_000),
            false,
        )
        .is_none());
    }

    #[test]
    fn nothing_to_gain_is_no_decision() {
        assert!(plan_output_size(
            &probe(1280, 720),
            FLAT,
            qs_size_limits("MaxWidth=1920&MaxHeight=1080"),
            None,
            false,
        )
        .is_none());
        assert!(plan_output_size(
            &MediaProbe::default(),
            FLAT,
            qs_size_limits("MaxWidth=640"),
            None,
            false,
        )
        .is_none());
    }

    #[test]
    fn a_full_sbs_downmix_is_bounded_on_one_eye() {
        // 3840x1080 FSBS is two 1920x1080 eyes: MaxWidth=1920 has nothing to do
        // (it used to give 960x540 after the crop halved the width again).
        let sbs = probe3d(3840, 1080, Video3DFormat::FullSideBySide);
        let l = qs_size_limits("MaxWidth=1920");
        assert!(plan_output_size(&sbs, StereoOutput::Mono2D, l, None, false).is_none());
        // 7680x2160 is two 3840x2160 eyes: scale the packed frame to 3840x1080
        // and the crop leaves 1920x1080.
        let big = probe3d(7680, 2160, Video3DFormat::FullSideBySide);
        let (s, _) = plan_output_size(&big, StereoOutput::Mono2D, l, None, false).unwrap();
        assert_eq!((s.width() / 2, s.height()), (1920, 1080));
        // Preserving the 3D layout bounds the packed frame as before.
        let (s, _) = plan_output_size(&sbs, StereoOutput::Preserve, l, None, false).unwrap();
        assert_eq!((s.width(), s.height()), (1920, 540));
    }

    #[test]
    fn a_full_tab_downmix_is_bounded_on_one_eye() {
        let tab = probe3d(1920, 2160, Video3DFormat::FullTopAndBottom);
        let l = qs_size_limits("MaxHeight=1080");
        assert!(plan_output_size(&tab, StereoOutput::Mono2D, l, None, false).is_none());
        let l = qs_size_limits("MaxHeight=540");
        let (s, _) = plan_output_size(&tab, StereoOutput::Mono2D, l, None, false).unwrap();
        assert_eq!((s.width(), s.height() / 2), (960, 540));
    }

    #[test]
    fn a_half_sbs_downmix_is_squeezed_back_so_the_packed_bound_applies() {
        let half = probe3d(3840, 1080, Video3DFormat::HalfSideBySide);
        let l = qs_size_limits("MaxWidth=1920");
        let (s, _) = plan_output_size(&half, StereoOutput::Mono2D, l, None, false).unwrap();
        assert_eq!((s.width(), s.height()), (1920, 540));
    }
}
