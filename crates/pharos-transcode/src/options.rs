//! Transcode option types. Independent of ffmpeg specifics so callers
//! reason in terms of containers/codecs the wire protocol exposes.

use pharos_core::time::Ticks;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    Mp4,
    Mkv,
    WebM,
    Mpegts,
    Mp3,
    Flac,
    Ogg,
    /// P11 — raw ADTS (AAC) stream. Used by `/Audio/{id}/universal`
    /// when remuxing FLAC / lossless sources to AAC for clients
    /// without FLAC decode.
    Adts,
    /// P38 — fragmented MP4 segment for HLSv6. Same mp4 muxer as
    /// `Container::Mp4` but the HLS handler picks a different
    /// `-hls_segment_type` and the master playlist bumps to
    /// `EXT-X-VERSION:6`. Safari + iOS native HLS prefer this; the
    /// MPEG-TS path stays default for everyone else.
    Fmp4,
}

impl Container {
    /// Whether this container is used to carry ONE HLS segment of a timeline
    /// tiled by independent per-segment transcodes.
    ///
    /// This is the property that decides whether a transcode needs
    /// frame-exact boundary timestamps: a segment's first frame has to land
    /// exactly where the previous segment's last frame ended, or the encoder
    /// resolves the sub-frame residue by duplicating or dropping the boundary
    /// frame — a hitch at every boundary, on every client. A progressive
    /// output has no boundaries and needs none of it.
    ///
    /// Derived from the container rather than re-tested per call site so a new
    /// segment container cannot be added without inheriting the treatment (the
    /// mpegts path was previously missed, so h264/TS segments never received
    /// `-enc_time_base` at all).
    pub fn is_hls_segment(self) -> bool {
        matches!(self, Container::Mpegts | Container::Fmp4)
    }

    /// ffmpeg `-f` muxer name.
    pub fn ffmpeg_muxer(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "matroska",
            Self::WebM => "webm",
            Self::Mpegts => "mpegts",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Adts => "adts",
            // fMP4 segments use the mp4 muxer with movflags tuned for
            // fragmentation; the HLS handler appends the flags.
            Self::Fmp4 => "mp4",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Mkv => "video/x-matroska",
            Self::WebM => "video/webm",
            Self::Mpegts => "video/mp2t",
            Self::Mp3 => "audio/mpeg",
            Self::Flac => "audio/flac",
            Self::Ogg => "audio/ogg",
            Self::Adts => "audio/aac",
            // RFC 6381 / Apple HLS Tech Note 281 — fMP4 segments are
            // `video/iso.segment`; the matching init segment is
            // `video/mp4` but the segment endpoint returns this.
            Self::Fmp4 => "video/iso.segment",
        }
    }

    /// Map a Jellyfin / ffprobe container token (lowercase) to the
    /// enum. Returns `None` for unknown / unsupported targets so the
    /// caller can fall back rather than 500.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mp4" | "m4v" => Some(Self::Mp4),
            // P38 — explicit fMP4 token; only handler code currently
            // surfaces this. Device profiles continue to emit "mp4".
            "fmp4" | "iso-segment" | "iso.segment" => Some(Self::Fmp4),
            "mkv" | "matroska" => Some(Self::Mkv),
            "webm" => Some(Self::WebM),
            "ts" | "mpegts" => Some(Self::Mpegts),
            "mp3" => Some(Self::Mp3),
            "flac" => Some(Self::Flac),
            "ogg" => Some(Self::Ogg),
            "aac" | "adts" => Some(Self::Adts),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
    Vp9,
    Av1,
    /// Pass-through; ffmpeg `-c:v copy`.
    Copy,
}

impl VideoCodec {
    pub fn ffmpeg_codec(self) -> &'static str {
        match self {
            Self::H264 => "libx264",
            Self::H265 => "libx265",
            Self::Vp9 => "libvpx-vp9",
            Self::Av1 => "libaom-av1",
            Self::Copy => "copy",
        }
    }

    /// Resolve a Jellyfin / probe codec name to the enum. Falls back
    /// to `None` for codecs ffmpeg in our build can't encode (e.g.
    /// proprietary HEVC variants without -enable-libx265).
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" | "avc1" => Some(Self::H264),
            "h265" | "hevc" => Some(Self::H265),
            "vp9" => Some(Self::Vp9),
            "av1" => Some(Self::Av1),
            "copy" => Some(Self::Copy),
            _ => None,
        }
    }
}

/// A SOURCE video codec — what a file already contains.
///
/// Deliberately a different type from [`VideoCodec`], which names what a
/// transcode TARGETS. The two answer different questions about different
/// hardware blocks: a GTX 1070 encodes H.264 and HEVC and decodes H.264, HEVC
/// and VP9, and it does neither for AV1 — so "this device can encode X" says
/// nothing about whether it can decode X, and a single enum shared between the
/// two invites exactly that substitution. `TranscodeOptions` carries one of
/// each, and mixing them up would be a compile error rather than a GPU asked
/// to decode a bitstream it has no block for.
///
/// The variants are the codecs a decode probe can build a sample for (every
/// one has a software encoder in this ffmpeg build). A source codec outside
/// this set parses to `None`, which the decode gate reads as "cannot be
/// proved decodable" and keeps on software — the safe direction, since the
/// cost of being wrong that way is a slower encode rather than a dead
/// rendition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceCodec {
    H264,
    Hevc,
    Vp9,
    Av1,
    Vp8,
    Mpeg2,
}

impl SourceCodec {
    /// Every variant, in the order the boot probe trials them.
    pub const ALL: [Self; 6] = [
        Self::H264,
        Self::Hevc,
        Self::Vp9,
        Self::Av1,
        Self::Vp8,
        Self::Mpeg2,
    ];

    /// Resolve an ffmpeg/probe `codec_name` (`MediaProbe::video_codec`) to the
    /// enum. `None` for anything this build cannot build a decode sample for.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" | "avc1" => Some(Self::H264),
            "hevc" | "h265" | "hvc1" | "hev1" => Some(Self::Hevc),
            "vp9" => Some(Self::Vp9),
            "av1" | "av01" => Some(Self::Av1),
            "vp8" => Some(Self::Vp8),
            "mpeg2video" | "mpeg2" => Some(Self::Mpeg2),
            _ => None,
        }
    }

    /// The ffmpeg decoder/codec name — also the sample file's extension.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
            Self::Vp8 => "vp8",
            Self::Mpeg2 => "mpeg2video",
        }
    }

    /// A SOFTWARE encoder for this codec, used to build the decode probe's
    /// sample. `libsvtav1` rather than `libaom-av1` (which
    /// [`VideoCodec::ffmpeg_codec`] names for real encodes): the sample is
    /// five frames of 128x128 and libaom would spend seconds on it at boot.
    pub fn sample_encoder(self) -> &'static str {
        match self {
            Self::H264 => "libx264",
            Self::Hevc => "libx265",
            Self::Vp9 => "libvpx-vp9",
            Self::Av1 => "libsvtav1",
            Self::Vp8 => "libvpx",
            Self::Mpeg2 => "mpeg2video",
        }
    }

    /// Stable, bounded label for a metric/log field. Renaming one breaks a
    /// dashboard silently, so the mapping is spelled out rather than derived
    /// from the variant name.
    pub fn label(self) -> &'static str {
        self.ffmpeg_name()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Aac,
    Mp3,
    Opus,
    Flac,
    Vorbis,
    /// Pass-through.
    Copy,
}

impl AudioCodec {
    pub fn ffmpeg_codec(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Mp3 => "libmp3lame",
            Self::Opus => "libopus",
            Self::Flac => "flac",
            Self::Vorbis => "libvorbis",
            Self::Copy => "copy",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "aac" | "mp4a" => Some(Self::Aac),
            "mp3" => Some(Self::Mp3),
            "opus" => Some(Self::Opus),
            "flac" => Some(Self::Flac),
            "vorbis" => Some(Self::Vorbis),
            "copy" => Some(Self::Copy),
            _ => None,
        }
    }
}

/// The title's one continuous audio encode, and where in the SOURCE its
/// first sample sits.
///
/// The start is not decoration. `-ss` on an input is relative to that
/// input's own start time, so seeking this file by an absolute source
/// position lands at `start + position` — measured on ffmpeg 8.1 with a
/// tone-marked fixture, a file spanning source 30..40 s served silence for
/// `-ss 32` and the correct content for `-ss 2`. A segment therefore has to
/// seek it by `position - start`, and cannot do that without knowing the
/// start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MuxedAudio {
    pub path: std::path::PathBuf,
    /// Source position of this file's first sample. 0 for a whole-title
    /// encode.
    pub start_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeOptions {
    pub container: Container,
    /// The SOURCE frame rate this segment's window was snapped to, when known.
    ///
    /// Carried because the HARDWARE encoders need it: they reject
    /// `-enc_time_base`, so without an explicit `-r` they quantize output to
    /// their default 1/framerate timebase, place the first frame half a frame
    /// late and lose the last one — one dropped frame per segment. See the
    /// `-r` block in `build_args_for_device`.
    #[serde(default)]
    pub source_frame_rate: Option<pharos_core::FrameRate>,
    pub video: Option<VideoCodec>,
    /// What the SOURCE contains, when it could be named.
    ///
    /// Carried purely so the scheduler can ask its device table whether the
    /// device it is about to place this job on can actually DECODE this — a
    /// question nothing used to ask, which is how a 10-bit AV1 file reached a
    /// GPU with no AV1 decode block and failed every frame. `None` means the
    /// source codec was not named (an unusual container, or a caller with no
    /// probe to hand) and reads as "do not offload the decode": the two ways of
    /// being wrong cost a dead rendition and a software decode respectively.
    ///
    /// Distinct from `video` above, which is the TARGET. They are different
    /// hardware blocks and a device answers about them independently.
    ///
    /// `#[serde(skip)]`, and that is load-bearing twice over. It keeps this
    /// field out of the wire frame, where the worker has no use for it — the
    /// worker is told the VERDICT ([`crate::protocol::JobSpec::decode_offload`]),
    /// not the input to it. And it keeps this field out of [`RenditionKey`],
    /// which digests these options through serde: the key decides which encoder
    /// a rendition pins to, nothing keyed on the HLS cache generation can see it
    /// move, and a field that adds no information (the source codec is a
    /// property of the input path, which the key already hashes) would
    /// nonetheless re-place two thirds of the live fMP4 renditions on deploy —
    /// serving cached segments from the previous encoder under an init the
    /// browser holds `immutable` for a year, which is issue #114 exactly.
    #[serde(skip)]
    pub source_video_codec: Option<SourceCodec>,
    pub audio: Option<AudioCodec>,
    pub video_bitrate_bps: Option<u64>,
    pub audio_bitrate_bps: Option<u64>,
    /// Jellyfin-style ticks (10,000,000 per second). 0 = start of stream.
    pub start_position_ticks: u64,
    /// Optional clip duration in Jellyfin ticks.
    pub duration_ticks: Option<u64>,
    /// Source-relative audio-stream index (`AudioStreamIndex` query
    /// param). When set, ffmpeg gets `-map 0:a:{N}` so multi-track
    /// sources transcode the chosen track instead of the default.
    /// None defers to ffmpeg's default selection.
    pub audio_source_stream_index: Option<u32>,
    /// Subtitle-relative stream index for IMAGE-subtitle burn-in
    /// (PGS/VOBSUB/DVB — the only kind callers request burn for; text subs
    /// are delivered out-of-band, ADR-0006). When set, ffmpeg gets a
    /// `-filter_complex "[0:v:0][0:s:N]overlay=…"` graph rendering the
    /// bitmap subtitle into the video frames (B40 — the text-only
    /// `subtitles=` filter cannot render image subs). None leaves subtitles
    /// out of the encode entirely.
    pub burn_subtitle_stream_index: Option<u32>,
    /// Whether the RENDITION this segment belongs to burns subtitles, as
    /// opposed to whether this segment does. Device placement reads this and
    /// never the per-segment index — see `SegmentOpts::burn_intent` (B200).
    ///
    /// Part of the `RenditionKey` canon, unlike the per-segment burn index
    /// which is zeroed there: a burning rendition and a non-burning one are
    /// genuinely different renditions and must be able to pin separately,
    /// while the two sides of a burn GATE are the same rendition and must not.
    ///
    /// Always serialized. `skip_serializing_if` would have kept every
    /// non-burning rendition's key byte-identical, but the worker protocol
    /// frames these options in a NON-SELF-DESCRIBING format, where an omitted
    /// field is a decode error rather than a default.
    pub burn_intent: bool,
    /// `true` when `burn_subtitle_stream_index` refers to a TEXT/ASS track
    /// (picks ffmpeg's `subtitles=` filter instead of the image `overlay`
    /// graph). `false` for image-subtitle burn or when no burn is set.
    pub burn_subtitle_is_text: bool,
    /// Local `.ass` sidecar to burn from for a TEXT/ASS burn. `Some` points
    /// the `subtitles=` filter at this small pre-extracted, disk-cached file
    /// (`filename=<p>`, no `si=` — a sidecar is single-track) instead of the
    /// whole source container. ffmpeg's `subtitles` filter opens a SECOND
    /// demuxer on `filename=` at init — ONCE PER SEGMENT — and reads the WHOLE
    /// container to collect subtitle packets + embedded fonts; pointing it at a
    /// multi-GB NFS source re-demuxes the entire file every 6 s segment (the
    /// documented whole-file-demux stutter). The sidecar preserves the source's
    /// ABSOLUTE event times, so the `setpts` alignment is unchanged. `None`
    /// falls back to `filename=<source>:si=N` (correct, just slower).
    pub burn_subtitle_ass_path: Option<std::path::PathBuf>,
    /// Directory of extracted embedded font attachments handed to libass as
    /// `:fontsdir=<dir>` alongside `burn_subtitle_ass_path`. libass scans it by
    /// font CONTENT, so index-named files are fine. `None` renders with
    /// system/default fonts. Only consulted on the TEXT/ASS-sidecar path.
    pub burn_fonts_dir: Option<std::path::PathBuf>,
    /// Overrides [`crate::DECODE_PREROLL_SECONDS`] — how far before
    /// `start_position_ticks` the decoder is seeded before the surplus is
    /// trimmed back off. `None` takes the default.
    ///
    /// Raised on a re-attempt when a produced segment came back short of
    /// frames: the container index claimed a random-access point the decoder
    /// could not actually start from, and the default preroll did not reach
    /// far enough back to find a real one.
    #[serde(default)]
    pub decode_preroll_seconds: Option<f64>,
    /// Take this segment's audio by COPY from the title's one continuous
    /// audio encode at this path, instead of encoding audio here.
    ///
    /// Encoding audio per segment re-primes the codec at every boundary: each
    /// segment emits its own priming frame and starts a fresh frame grid at
    /// its own seek point, so consecutive segments carry overlapping,
    /// phase-misaligned audio. Copying from one encode leaves a single global
    /// grid, so every audio frame belongs to exactly one segment at a
    /// deterministic timestamp and there is nothing to drift against.
    ///
    /// The file is added as a second input seeked to the SAME position as the
    /// source, because two inputs seeked to different positions are re-based
    /// by different amounts and the audio would land offset from the video by
    /// the difference.
    #[serde(default)]
    pub muxed_audio_source: Option<MuxedAudio>,
}

impl TranscodeOptions {
    pub fn start_position_seconds(&self) -> Option<f64> {
        if self.start_position_ticks == 0 {
            None
        } else {
            Some(Ticks(self.start_position_ticks).seconds())
        }
    }

    pub fn duration_seconds(&self) -> Option<f64> {
        self.duration_ticks.map(|d| Ticks(d).seconds())
    }
}

/// Identifies a set of segments that MUST come from one encoder because they
/// share one init (T105 / spec 003).
///
/// A shared-init fMP4 rendition serves every segment under a single `avcC`
/// carrying seg0's parameter sets, and the media segments carry none of their
/// own. Two encoders' SPS are not interchangeable — measured on the deployment
/// GPU, NVENC emits Main profile where libx264 emits High — so a segment
/// produced by a different encoder than the init is undecodable (issue #114).
///
/// The key is DERIVED from the whole `TranscodeOptions`, with only the fields
/// that legitimately vary WITHIN a rendition neutralised. That direction
/// matters: a hand-listed key silently goes stale when a new option is added,
/// and the failure mode is corrupt video rather than a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RenditionKey(u64);

impl RenditionKey {
    /// Fields neutralised here are the ones that differ between segments of the
    /// SAME rendition. Everything else — codec, container, bitrate, audio
    /// track, burn variant, frame rate — distinguishes renditions that cannot
    /// share an init.
    ///
    /// A field that is FUNCTIONALLY DETERMINED by `input` (hashed separately
    /// below) does not belong in the digest at all, and is kept out with
    /// `#[serde(skip)]` at its declaration rather than cleared here — clearing
    /// is not enough, because serde still emits the key and the resulting
    /// canon, and therefore the pin, still moves. See
    /// [`TranscodeOptions::source_video_codec`] for what that costs.
    pub fn new(input: &std::path::Path, opts: &TranscodeOptions) -> Self {
        let mut o = opts.clone();
        o.start_position_ticks = 0;
        o.duration_ticks = None;
        o.decode_preroll_seconds = None;
        o.burn_subtitle_stream_index = None;
        // PER-SEGMENT, like the position fields above: `maybe_gate_burn` clears
        // this for any segment whose window carries no subtitle event, so two
        // segments of ONE rendition disagree on it. Left in the canon it split
        // the rendition in half — the dialogue segments hashing to one key and
        // the quiet ones to another, each free to resolve to a different
        // encoder, which is undecodable under a shared init (B200/V153, #114).
        // The rendition-level truth lives in `burn_intent`, which placement
        // reads directly.
        if let Some(m) = o.muxed_audio_source.as_mut() {
            // The muxed-audio PATH is part of the rendition; the offset into it
            // is per-segment.
            m.start_seconds = 0.0;
        }
        // Canonicalised through serde so a newly added option joins the key
        // automatically. Falling back to the Debug form keeps this total —
        // there is no sensible way to fail here, and a key that panicked would
        // take playback down.
        let canon = serde_json::to_string(&o).unwrap_or_else(|_| format!("{o:?}"));

        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        input.hash(&mut h);
        canon.hash(&mut h);
        Self(h.finish())
    }

    /// The raw value, for deterministic device selection.
    pub fn value(&self) -> u64 {
        self.0
    }

    /// Short label for logs and metrics.
    pub fn short(&self) -> String {
        format!("{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn ticks_to_seconds_roundtrip() {
        let o = TranscodeOptions {
            source_video_codec: None,
            source_frame_rate: None,
            container: Container::Mp4,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 30_000_000,
            duration_ticks: Some(50_000_000),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
        };
        assert_eq!(o.start_position_seconds(), Some(3.0));
        assert_eq!(o.duration_seconds(), Some(5.0));
    }

    #[test]
    fn zero_start_returns_none() {
        let o = TranscodeOptions {
            source_video_codec: None,
            source_frame_rate: None,
            container: Container::Mp4,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
        };
        assert_eq!(o.start_position_seconds(), None);
    }

    #[test]
    fn container_content_types_match_jellyfin_expectations() {
        assert_eq!(Container::Mp4.content_type(), "video/mp4");
        assert_eq!(Container::Mpegts.content_type(), "video/mp2t");
        assert_eq!(Container::Mp3.content_type(), "audio/mpeg");
    }

    #[test]
    fn fmp4_container_muxes_as_mp4_with_segment_type() {
        // P38 — the muxer name has to stay "mp4" so ffmpeg pipes the
        // bytes through the same demuxer the HLS handler initialises
        // its `-movflags` for. The wire-shape content-type swap to
        // `video/iso.segment` is what tells Safari it's HLSv6.
        assert_eq!(Container::Fmp4.ffmpeg_muxer(), "mp4");
        assert_eq!(Container::Fmp4.content_type(), "video/iso.segment");
        assert_eq!(Container::from_name("fmp4"), Some(Container::Fmp4));
        assert_eq!(Container::from_name("iso-segment"), Some(Container::Fmp4));
        // "mp4" itself stays the regular mp4 progressive container so
        // device-profile parsers don't accidentally upgrade clients
        // that asked for plain mp4.
        assert_eq!(Container::from_name("mp4"), Some(Container::Mp4));
    }

    fn cmaf_opts() -> TranscodeOptions {
        TranscodeOptions {
            source_video_codec: None,
            container: Container::Fmp4,
            source_frame_rate: None,
            video: Some(VideoCodec::H264),
            audio: None,
            video_bitrate_bps: Some(3_000_000),
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
        }
    }

    /// Segments of ONE rendition must share a key: only the per-segment fields
    /// differ between them, and if those leaked into the key every segment
    /// would pin its own device and the guarantee would be vacuous.
    #[test]
    fn segments_of_one_rendition_share_a_key() {
        let path = std::path::Path::new("/media/Show/S01E01.mkv");
        let seg0 = cmaf_opts();
        let mut seg7 = cmaf_opts();
        seg7.start_position_ticks = 7 * 6 * 10_000_000;
        seg7.duration_ticks = Some(6 * 10_000_000);
        seg7.decode_preroll_seconds = Some(1.5);
        assert_eq!(
            RenditionKey::new(path, &seg0),
            RenditionKey::new(path, &seg7),
            "start/duration/preroll vary WITHIN a rendition and must not split it"
        );
    }

    /// Anything that changes the encode's parameter sets is a different
    /// rendition and must not be allowed to share an init.
    #[test]
    fn an_encode_affecting_field_changes_the_key() {
        let path = std::path::Path::new("/media/Show/S01E01.mkv");
        let base = RenditionKey::new(path, &cmaf_opts());

        let mut o = cmaf_opts();
        o.video_bitrate_bps = Some(6_000_000);
        assert_ne!(base, RenditionKey::new(path, &o), "bitrate rung");

        let mut o = cmaf_opts();
        o.container = Container::Mpegts;
        assert_ne!(base, RenditionKey::new(path, &o), "container");

        let mut o = cmaf_opts();
        o.audio_source_stream_index = Some(2);
        assert_ne!(base, RenditionKey::new(path, &o), "audio track");

        // The burn VARIANT is expressed by the rendition-level intent, not by
        // the per-segment index: the index is gated off for any segment with
        // no subtitle event in its window, so keying on it split one rendition
        // into two that could resolve to different encoders (B200/V153).
        let mut o = cmaf_opts();
        o.burn_intent = true;
        o.burn_subtitle_stream_index = Some(1);
        assert_ne!(base, RenditionKey::new(path, &o), "burn variant");

        // …and the gate must NOT split it: same rendition, one segment with a
        // subtitle in it and one without.
        let mut burning = cmaf_opts();
        burning.burn_intent = true;
        burning.burn_subtitle_stream_index = Some(1);
        let mut gated = burning.clone();
        gated.burn_subtitle_stream_index = None;
        assert_eq!(
            RenditionKey::new(path, &burning),
            RenditionKey::new(path, &gated),
            "the burn gate varies WITHIN a rendition and must not split it"
        );

        let mut o = cmaf_opts();
        o.source_frame_rate = pharos_core::FrameRate::from_mille(23_976);
        assert_ne!(base, RenditionKey::new(path, &o), "frame rate");

        assert_ne!(
            base,
            RenditionKey::new(std::path::Path::new("/media/Show/S01E02.mkv"), &cmaf_opts()),
            "a different source is a different rendition"
        );
    }

    #[test]
    fn video_codec_maps_to_known_ffmpeg_lib() {
        assert_eq!(VideoCodec::H264.ffmpeg_codec(), "libx264");
        assert_eq!(VideoCodec::Av1.ffmpeg_codec(), "libaom-av1");
        assert_eq!(VideoCodec::Copy.ffmpeg_codec(), "copy");
    }

    #[test]
    fn audio_codec_maps_to_known_ffmpeg_lib() {
        assert_eq!(AudioCodec::Aac.ffmpeg_codec(), "aac");
        assert_eq!(AudioCodec::Opus.ffmpeg_codec(), "libopus");
    }

    /// A field that is determined by the input path must not move the rendition
    /// key, and one that genuinely distinguishes renditions must.
    ///
    /// The first half is the load-bearing one. `RenditionKey` decides which
    /// ENCODER a rendition pins to, and nothing keyed on the HLS cache
    /// generation observes it — so a key that shifts for no new information
    /// re-places live renditions with a warm cache and an unchanged `g=` in
    /// every client's year-long `immutable` init. Adding
    /// `source_video_codec` did exactly that until it was neutralised here,
    /// and two placement tests caught it only by accident.
    #[test]
    fn only_fields_that_distinguish_renditions_move_the_key() {
        let path = std::path::Path::new("/m/x.mkv");
        let base = TranscodeOptions {
            container: Container::Fmp4,
            source_frame_rate: None,
            video: Some(VideoCodec::H264),
            source_video_codec: None,
            audio: None,
            video_bitrate_bps: Some(3_000_000),
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
        };
        let mut named = base.clone();
        named.source_video_codec = Some(SourceCodec::Av1);
        assert_eq!(
            RenditionKey::new(path, &base),
            RenditionKey::new(path, &named),
            "the source codec is a property of the path, which is already in \
             the key; letting it in re-places every rendition for nothing"
        );

        // The neutralisation must be surgical, not a blanket "ignore new
        // fields": a rendition that targets a different codec still cannot
        // share an init with this one.
        let mut other_target = base.clone();
        other_target.video = Some(VideoCodec::H265);
        assert_ne!(
            RenditionKey::new(path, &base),
            RenditionKey::new(path, &other_target)
        );
        // And two different files are two different renditions.
        assert_ne!(
            RenditionKey::new(path, &base),
            RenditionKey::new(std::path::Path::new("/m/y.mkv"), &base)
        );
    }
}
