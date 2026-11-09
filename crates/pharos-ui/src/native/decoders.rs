//! What the Android TV app can play, from the device's decoder list
//! (spec 032 slice 3).
//!
//! The browser build asks `MediaSource.isTypeSupported` about each codec
//! (`playback::capabilities`). The Dioxus Native build has no browser to ask:
//! the platform layer lists the device's decoders through
//! `android.media.MediaCodecList` and hands them here, and this decides which
//! of them the server may send. Kept pure so the rules are tested on the host;
//! the JNI walk only copies fields.

use pharos_client::api::Capabilities;

use super::tv_page::NO_DIRECT_PLAY;

/// One decoder as `MediaCodecList` describes it, for one MIME type. A codec
/// that takes several types is listed once per type. Encoders are never
/// listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderInfo {
    /// `MediaCodecInfo.getName()`, such as `c2.mtk.avc.decoder`.
    pub name: String,
    /// The MIME type, such as `video/avc`.
    pub mime: String,
    /// `MediaCodecInfo.isHardwareAccelerated()` (API 29). Older devices
    /// report `false`; [`DecoderInfo::is_hardware`] then reads the name.
    pub hardware: bool,
    /// The largest picture the decoder takes, from
    /// `VideoCapabilities.getSupportedWidths/Heights().getUpper()`; 0 for an
    /// audio decoder.
    pub max_width: u32,
    pub max_height: u32,
}

impl DecoderInfo {
    /// Whether the decoder runs in dedicated hardware. Android's own software
    /// decoders are named `c2.android.*` (Codec2) or `OMX.google.*` (OMX);
    /// a vendor decoder that predates `isHardwareAccelerated` is hardware.
    pub fn is_hardware(&self) -> bool {
        let software = self.name.starts_with("c2.android.")
            || self.name.starts_with("OMX.google.")
            || self.name.contains(".sw.");
        self.hardware || !software
    }
}

/// The picture a video decoder must take to be offered: the server's
/// transcodes top out at 1080p, and a decoder that cannot take that would
/// fail mid-title on the first 1080p source it is sent.
pub const MIN_VIDEO_SIZE: (u32, u32) = (1920, 1080);

/// Server codec name for each video MIME type `MediaCodecList` uses, in the
/// order the browser build lists them.
const VIDEO_MIMES: &[(&str, &str)] = &[
    ("h264", "video/avc"),
    ("hevc", "video/hevc"),
    ("vp9", "video/x-vnd.on2.vp9"),
    ("av1", "video/av01"),
];

/// Server codec name for each audio MIME type.
const AUDIO_MIMES: &[(&str, &str)] = &[
    ("aac", "audio/mp4a-latm"),
    ("opus", "audio/opus"),
    ("flac", "audio/flac"),
    ("mp3", "audio/mpeg"),
    ("ac3", "audio/ac3"),
    ("eac3", "audio/eac3"),
];

/// The capabilities to negotiate with, from the device's decoders.
///
/// * Video: a codec is offered only when a **hardware** decoder takes at
///   least [`MIN_VIDEO_SIZE`]. A 32-bit TV SoC cannot be relied on to decode
///   1080p VP9 or AV1 in software in real time, and Android ships software
///   decoders for both on every device, so offering one would trade a server
///   transcode for a stuttering picture.
/// * Audio: any decoder, software included; audio decode is cheap.
/// * Containers: [`NO_DIRECT_PLAY`] only. The native player plays HLS fMP4
///   through `pharos-hls`, so the server must never pick direct play of a
///   file, and the negotiator reads an empty list as "any container".
/// * An empty codec list would also mean "any" to the negotiator, so a probe
///   that finds nothing (JNI failed, or a list with no usable decoder) falls
///   back to H.264 and AAC, the decoders Android's CDD requires on every TV.
/// * Image subtitles: none until the native player draws the server's PNG
///   cues; until then the server burns them in.
/// * HDR: what the display reports, passed in by the platform layer.
pub fn capabilities(decoders: &[DecoderInfo], display_hdr: bool) -> Capabilities {
    let video = |mime: &str| {
        decoders.iter().any(|d| {
            d.mime.eq_ignore_ascii_case(mime)
                && d.is_hardware()
                && d.max_width >= MIN_VIDEO_SIZE.0
                && d.max_height >= MIN_VIDEO_SIZE.1
        })
    };
    let audio = |mime: &str| decoders.iter().any(|d| d.mime.eq_ignore_ascii_case(mime));
    let pick = |table: &[(&str, &str)], ok: &dyn Fn(&str) -> bool| -> Vec<String> {
        table
            .iter()
            .filter(|(_, mime)| ok(mime))
            .map(|(name, _)| (*name).to_owned())
            .collect()
    };
    let or_required = |mut found: Vec<String>, required: &str| {
        if found.is_empty() {
            found.push(required.to_owned());
        }
        found
    };
    Capabilities {
        video_codecs: or_required(pick(VIDEO_MIMES, &video), "h264"),
        audio_codecs: or_required(pick(AUDIO_MIMES, &audio), "aac"),
        containers: vec![NO_DIRECT_PLAY.to_owned()],
        max_bitrate: 0,
        hdr: display_hdr,
        image_subtitles: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(name: &str, mime: &str, hardware: bool, size: (u32, u32)) -> DecoderInfo {
        DecoderInfo {
            name: name.to_owned(),
            mime: mime.to_owned(),
            hardware,
            max_width: size.0,
            max_height: size.1,
        }
    }

    const UHD: (u32, u32) = (4096, 2176);

    /// A TV with vendor decoders for H.264, HEVC and VP9 but only Android's
    /// software AV1, and software audio.
    fn mediatek_tv() -> Vec<DecoderInfo> {
        vec![
            dec("c2.mtk.avc.decoder", "video/avc", true, UHD),
            dec("c2.mtk.hevc.decoder", "video/hevc", true, UHD),
            dec("c2.mtk.vp9.decoder", "video/x-vnd.on2.vp9", true, UHD),
            dec("c2.android.av1.decoder", "video/av01", false, UHD),
            dec("c2.android.avc.decoder", "video/avc", false, UHD),
            dec("c2.android.aac.decoder", "audio/mp4a-latm", false, (0, 0)),
            dec("c2.android.opus.decoder", "audio/opus", false, (0, 0)),
            dec("c2.android.mp3.decoder", "audio/mpeg", false, (0, 0)),
            dec("c2.android.flac.decoder", "audio/flac", false, (0, 0)),
        ]
    }

    #[test]
    fn video_needs_a_hardware_decoder() {
        let caps = capabilities(&mediatek_tv(), false);
        assert_eq!(
            caps.video_codecs,
            ["h264", "hevc", "vp9"],
            "AV1 has only Android's software decoder, which the TV cannot run at 1080p"
        );
    }

    /// The G08's own list (`dumpsys media.player`, 2026-11-09), cut to the
    /// types the tables name: MediaTek hardware for every video codec,
    /// AV1 included, beside Android's software ones; Dolby in hardware.
    fn g08() -> Vec<DecoderInfo> {
        let sw = |name: &str, mime: &str, size| dec(name, mime, false, size);
        vec![
            dec("c2.mtk.av1.decoder", "video/av01", true, (4096, 2304)),
            sw("c2.android.av1.decoder", "video/av01", (1920, 1080)),
            dec("c2.mtk.avc.decoder", "video/avc", true, (4096, 2304)),
            sw("c2.android.avc.decoder", "video/avc", (4080, 4080)),
            dec("c2.mtk.hevc.decoder", "video/hevc", true, (4096, 2304)),
            dec(
                "c2.mtk.vp9.decoder",
                "video/x-vnd.on2.vp9",
                true,
                (4096, 2304),
            ),
            sw(
                "c2.android.vp9.decoder",
                "video/x-vnd.on2.vp9",
                (2048, 2048),
            ),
            dec("c2.mtk.ac3.decoder", "audio/ac3", true, (0, 0)),
            dec("c2.mtk.ac3.decoder", "audio/eac3", true, (0, 0)),
            sw("c2.android.aac.decoder", "audio/mp4a-latm", (0, 0)),
            sw("c2.android.flac.decoder", "audio/flac", (0, 0)),
            sw("c2.android.mp3.decoder", "audio/mpeg", (0, 0)),
            sw("c2.android.opus.decoder", "audio/opus", (0, 0)),
        ]
    }

    #[test]
    fn the_g08_is_offered_every_codec_the_server_sends() {
        let caps = capabilities(&g08(), false);
        assert_eq!(caps.video_codecs, ["h264", "hevc", "vp9", "av1"]);
        assert_eq!(
            caps.audio_codecs,
            ["aac", "opus", "flac", "mp3", "ac3", "eac3"]
        );
    }

    #[test]
    fn audio_takes_software_decoders_and_offers_dolby_only_when_present() {
        let caps = capabilities(&mediatek_tv(), false);
        assert_eq!(caps.audio_codecs, ["aac", "opus", "flac", "mp3"]);

        let mut list = mediatek_tv();
        list.push(dec("c2.dolby.ac3.decoder", "audio/ac3", true, (0, 0)));
        list.push(dec("c2.dolby.eac3.decoder", "audio/eac3", true, (0, 0)));
        let caps = capabilities(&list, false);
        assert_eq!(
            caps.audio_codecs,
            ["aac", "opus", "flac", "mp3", "ac3", "eac3"]
        );
    }

    #[test]
    fn a_hardware_decoder_below_1080p_is_not_offered() {
        let list = [
            dec("c2.mtk.avc.decoder", "video/avc", true, UHD),
            dec(
                "OMX.vendor.vp9.decoder",
                "video/x-vnd.on2.vp9",
                true,
                (1280, 720),
            ),
        ];
        assert_eq!(capabilities(&list, false).video_codecs, ["h264"]);
    }

    #[test]
    fn a_vendor_decoder_without_the_hardware_flag_counts_as_hardware() {
        // isHardwareAccelerated() is API 29; before it every codec says false.
        let list = [dec("OMX.MTK.VIDEO.DECODER.HEVC", "video/hevc", false, UHD)];
        assert_eq!(capabilities(&list, false).video_codecs, ["hevc"]);
        let sw = [dec("OMX.google.hevc.decoder", "video/hevc", false, UHD)];
        assert_eq!(
            capabilities(&sw, false).video_codecs,
            ["h264"],
            "only the CDD fallback"
        );
    }

    #[test]
    fn mime_case_and_unknown_types_do_not_matter() {
        let list = [
            dec("c2.mtk.hevc.decoder", "VIDEO/HEVC", true, UHD),
            dec("c2.mtk.mpeg2.decoder", "video/mpeg2", true, UHD),
            dec("c2.android.opus.decoder", "Audio/Opus", false, (0, 0)),
        ];
        let caps = capabilities(&list, false);
        assert_eq!(caps.video_codecs, ["hevc"]);
        assert_eq!(caps.audio_codecs, ["opus"]);
    }

    #[test]
    fn the_native_player_asks_for_hls_only_and_burned_image_subtitles() {
        let caps = capabilities(&mediatek_tv(), true);
        assert_eq!(
            caps.containers,
            [NO_DIRECT_PLAY],
            "no direct play of a file; an empty list would mean any container"
        );
        assert!(caps.image_subtitles.is_empty(), "image cues are burned in");
        assert!(caps.hdr, "HDR comes from the display, not the decoders");
        assert_eq!(caps.max_bitrate, 0);
    }

    #[test]
    fn an_empty_probe_never_declares_an_empty_codec_list() {
        // The negotiator reads an empty list as "any codec", which would send
        // HEVC 10-bit to a TV whose probe merely failed.
        let caps = capabilities(&[], false);
        assert_eq!(caps.video_codecs, ["h264"]);
        assert_eq!(caps.audio_codecs, ["aac"]);
    }
}
