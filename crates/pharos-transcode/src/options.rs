//! Transcode option types. Independent of ffmpeg specifics so callers
//! reason in terms of containers/codecs the wire protocol exposes.

use pharos_core::time::Ticks;
use pharos_core::Video3DFormat;
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

/// What the source's pixels look like, as far as the GPU-resident VAAPI path
/// cares: that path hands decoded surfaces straight to the encoder, so it is
/// only right for plain 8-bit SDR 4:2:0 with even dimensions. Anything else
/// takes the CPU-filter path, which converts and pads in software.
///
/// `Unknown` is the default and reads as "not plain": a caller with no probe
/// to hand must never get the faster path by accident. `#[serde(skip)]` on
/// [`TranscodeOptions::source_video_kind`] for the same reason as
/// `source_video_codec`: it is a property of the input, which the rendition key
/// already hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceVideoKind {
    #[default]
    Unknown,
    /// 8-bit 4:2:0 SDR, even width and height.
    Plain8BitSdr,
    /// Known, and not plain: 10-bit, 4:2:2/4:4:4, HDR transfer or odd size.
    Other,
}

impl SourceVideoKind {
    /// Classify from the probe's fields (`MediaProbe::pixel_format`,
    /// `color_transfer`, `width`, `height`). Any missing field is `Unknown`.
    pub fn classify(
        pixel_format: Option<&str>,
        color_transfer: Option<&str>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Self {
        let (Some(pix), Some(w), Some(h)) = (pixel_format, width, height) else {
            return Self::Unknown;
        };
        let eight_bit_420 = matches!(pix.trim(), "yuv420p" | "nv12");
        let hdr = matches!(
            color_transfer
                .map(|t| t.trim().to_ascii_lowercase())
                .as_deref(),
            Some("smpte2084" | "arib-std-b67")
        );
        let even = w % 2 == 0 && h % 2 == 0;
        if eight_bit_420 && !hdr && even {
            Self::Plain8BitSdr
        } else {
            Self::Other
        }
    }
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

#[derive(Debug, Clone)]
pub struct TranscodeOptions {
    pub container: Container,
    /// The SOURCE frame rate this segment's window was snapped to, when known.
    ///
    /// Carried because the HARDWARE encoders need it: they reject
    /// `-enc_time_base`, so without an explicit `-r` they quantize output to
    /// their default 1/framerate timebase, place the first frame half a frame
    /// late and lose the last one — one dropped frame per segment. See the
    /// `-r` block in `build_args_for_device`.
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
    pub source_video_codec: Option<SourceCodec>,
    /// Whether the source is plain 8-bit SDR; see [`SourceVideoKind`]. Read by
    /// the scheduler to decide the VAAPI pipeline, never by the worker.
    pub source_video_kind: SourceVideoKind,
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
    pub muxed_audio_source: Option<MuxedAudio>,
    /// B228 — when set, this segment is a FILLER: black video (+ silence for
    /// a muxed audio rendition) generated from an ffmpeg `lavfi` source
    /// instead of decoded from the real input, because the retry ladder
    /// (max decode preroll) still came back short of — or with zero — video
    /// frames. The frames are not in the source; no further retry conjures
    /// them (V171).
    ///
    /// `#[serde(skip)]`, exactly like [`Self::source_video_codec`] and for
    /// the same reason: `RenditionKey` digests these options through serde,
    /// and this field is set on only SOME segments of a rendition — the ones
    /// that happened to land in a damaged span. A NON-skip field, even one
    /// unconditionally neutralised in `RenditionKey::new`, still changes the
    /// JSON text every OTHER rendition in the system serializes to (a bare
    /// `,"filler":null` lands in every canon that never had it), which
    /// shifts every hash-derived device placement at once — measured live
    /// while building this fix: two placement tests broke from that field
    /// alone existing, non-skip, with a value that never varied. `skip`
    /// avoids the byte ever appearing, so an untouched rendition's canon —
    /// and therefore its pinned device — is provably unchanged.
    ///
    /// This is why the WIRE delivery to the worker (a separate process, so
    /// this in-process field alone cannot reach it) does NOT go through this
    /// field at all: see [`crate::protocol::JobSpec::filler`], which travels
    /// beside `decode_offload` — a fact ABOUT the job, not part of the
    /// rendition's own identity — for exactly the reason that field does.
    /// This field exists purely for the CALLER convenience of attaching a
    /// filler decision to a `TranscodeOptions` value before the scheduler
    /// pulls it back out into that sibling field (see
    /// `HlsSegmentCache::write_segment`); nothing downstream reads it after
    /// that point.
    pub filler: Option<FillerSpec>,
    /// 3D video layout if source is stereoscopic.
    pub video_3d_format: Option<pharos_core::Video3DFormat>,
    /// Target stereoscopic output mode (preserve or downmix to 2D mono).
    pub stereo_output: StereoOutput,
}

/// What the spawn arg builder needs to synthesize a filler segment: the
/// rendition's own frame size (so the black video matches every sibling
/// segment's dimensions — nothing here is guessed) and, when the rendition
/// muxes audio, which codec to encode `anullsrc` silence with.
///
/// Carries no bitrate of its own: `TranscodeOptions::audio_bitrate_bps` is
/// already the right value for a muxed rendition regardless of whether this
/// particular segment encodes or copies audio, so filler reuses it rather
/// than duplicating it here.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FillerSpec {
    pub width: u32,
    pub height: u32,
    /// `Some` only for a rendition whose segments mux audio
    /// (`muxed_audio_source.is_some()` on the real attempt) — the silence
    /// substitutes for the COPY that attempt would have made from the
    /// title's continuous audio encode. `None` for a video-only (demuxed
    /// audio) rendition, which gets no audio input at all, same as a real
    /// segment of that rendition.
    pub audio_codec: Option<AudioCodec>,
}

#[derive(Serialize, Deserialize)]
struct TranscodeOptionsBinary {
    container: Container,
    #[serde(default)]
    source_frame_rate: Option<pharos_core::FrameRate>,
    #[serde(skip)]
    source_video_codec: Option<SourceCodec>,
    #[serde(skip)]
    source_video_kind: SourceVideoKind,
    video: Option<VideoCodec>,
    audio: Option<AudioCodec>,
    video_bitrate_bps: Option<u64>,
    audio_bitrate_bps: Option<u64>,
    start_position_ticks: u64,
    duration_ticks: Option<u64>,
    audio_source_stream_index: Option<u32>,
    burn_subtitle_stream_index: Option<u32>,
    burn_intent: bool,
    burn_subtitle_is_text: bool,
    burn_subtitle_ass_path: Option<std::path::PathBuf>,
    burn_fonts_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    decode_preroll_seconds: Option<f64>,
    #[serde(default)]
    muxed_audio_source: Option<MuxedAudio>,
    video_3d_format: Option<pharos_core::Video3DFormat>,
    stereo_output: StereoOutput,
}

#[derive(Serialize, Deserialize)]
struct TranscodeOptionsJson {
    container: Container,
    #[serde(default)]
    source_frame_rate: Option<pharos_core::FrameRate>,
    #[serde(skip)]
    source_video_codec: Option<SourceCodec>,
    #[serde(skip)]
    source_video_kind: SourceVideoKind,
    video: Option<VideoCodec>,
    audio: Option<AudioCodec>,
    video_bitrate_bps: Option<u64>,
    audio_bitrate_bps: Option<u64>,
    start_position_ticks: u64,
    duration_ticks: Option<u64>,
    audio_source_stream_index: Option<u32>,
    burn_subtitle_stream_index: Option<u32>,
    burn_intent: bool,
    burn_subtitle_is_text: bool,
    burn_subtitle_ass_path: Option<std::path::PathBuf>,
    burn_fonts_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    decode_preroll_seconds: Option<f64>,
    #[serde(default)]
    muxed_audio_source: Option<MuxedAudio>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    video_3d_format: Option<pharos_core::Video3DFormat>,
    #[serde(default, skip_serializing_if = "StereoOutput::is_preserve")]
    stereo_output: StereoOutput,
}

macro_rules! impl_transcode_options_shim {
    ($shim:ident) => {
        impl From<&TranscodeOptions> for $shim {
            fn from(o: &TranscodeOptions) -> Self {
                Self {
                    container: o.container,
                    source_frame_rate: o.source_frame_rate,
                    source_video_codec: o.source_video_codec,
                    source_video_kind: o.source_video_kind,
                    video: o.video,
                    audio: o.audio,
                    video_bitrate_bps: o.video_bitrate_bps,
                    audio_bitrate_bps: o.audio_bitrate_bps,
                    start_position_ticks: o.start_position_ticks,
                    duration_ticks: o.duration_ticks,
                    audio_source_stream_index: o.audio_source_stream_index,
                    burn_subtitle_stream_index: o.burn_subtitle_stream_index,
                    burn_intent: o.burn_intent,
                    burn_subtitle_is_text: o.burn_subtitle_is_text,
                    burn_subtitle_ass_path: o.burn_subtitle_ass_path.clone(),
                    burn_fonts_dir: o.burn_fonts_dir.clone(),
                    decode_preroll_seconds: o.decode_preroll_seconds,
                    muxed_audio_source: o.muxed_audio_source.clone(),
                    video_3d_format: o.video_3d_format,
                    stereo_output: o.stereo_output,
                }
            }
        }

        impl From<$shim> for TranscodeOptions {
            fn from(o: $shim) -> Self {
                Self {
                    container: o.container,
                    source_frame_rate: o.source_frame_rate,
                    source_video_codec: o.source_video_codec,
                    source_video_kind: o.source_video_kind,
                    video: o.video,
                    audio: o.audio,
                    video_bitrate_bps: o.video_bitrate_bps,
                    audio_bitrate_bps: o.audio_bitrate_bps,
                    start_position_ticks: o.start_position_ticks,
                    duration_ticks: o.duration_ticks,
                    audio_source_stream_index: o.audio_source_stream_index,
                    burn_subtitle_stream_index: o.burn_subtitle_stream_index,
                    burn_intent: o.burn_intent,
                    burn_subtitle_is_text: o.burn_subtitle_is_text,
                    burn_subtitle_ass_path: o.burn_subtitle_ass_path,
                    burn_fonts_dir: o.burn_fonts_dir,
                    decode_preroll_seconds: o.decode_preroll_seconds,
                    muxed_audio_source: o.muxed_audio_source,
                    video_3d_format: o.video_3d_format,
                    stereo_output: o.stereo_output,
                    filler: None,
                }
            }
        }
    };
}

impl_transcode_options_shim!(TranscodeOptionsBinary);
impl_transcode_options_shim!(TranscodeOptionsJson);

impl Serialize for TranscodeOptions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            TranscodeOptionsJson::from(self).serialize(serializer)
        } else {
            TranscodeOptionsBinary::from(self).serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for TranscodeOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            TranscodeOptionsJson::deserialize(deserializer).map(Into::into)
        } else {
            TranscodeOptionsBinary::deserialize(deserializer).map(Into::into)
        }
    }
}

impl Default for TranscodeOptions {
    fn default() -> Self {
        Self {
            container: Container::Mp4,
            source_frame_rate: None,
            source_video_codec: None,
            source_video_kind: SourceVideoKind::default(),
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
            video_3d_format: None,
            stereo_output: StereoOutput::Preserve,
            filler: None,
        }
    }
}

/// Stereoscopic downmix mode for transcoding (spec 009-3d-media).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StereoOutput {
    #[default]
    Preserve,
    #[serde(rename = "mono2d")]
    Mono2D,
}

impl StereoOutput {
    pub const ALL: [Self; 2] = [Self::Preserve, Self::Mono2D];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Mono2D => "mono2d",
        }
    }

    pub const fn label(self) -> &'static str {
        self.as_str()
    }

    pub const fn is_preserve(&self) -> bool {
        matches!(self, Self::Preserve)
    }
}

/// ffmpeg video filter string to extract and display the left eye as a 2D frame.
pub fn stereo_mono_filter(format: Video3DFormat) -> Option<&'static str> {
    match format {
        Video3DFormat::HalfSideBySide => Some("crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1"),
        Video3DFormat::FullSideBySide => Some("crop=iw/2:ih:0:0,setsar=1"),
        Video3DFormat::HalfTopAndBottom => Some("crop=iw:ih/2:0:0,scale=iw:ih*2,setsar=1"),
        Video3DFormat::FullTopAndBottom => Some("crop=iw:ih/2:0:0,setsar=1"),
        Video3DFormat::Mvc => None,
    }
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

    /// Resolves the effective stereoscopic output mode.
    ///
    /// Returns [`StereoOutput::Mono2D`] when requested explicitly OR when burning
    /// subtitles onto packed SBS/TAB video (B230) so the subtitle is not split
    /// across both eyes. Otherwise preserves stereoscopic layout.
    pub fn effective_stereo_output(&self) -> StereoOutput {
        if self.stereo_output == StereoOutput::Mono2D
            || ((self.burn_subtitle_stream_index.is_some() || self.burn_intent)
                && self
                    .video_3d_format
                    .is_some_and(|f| f.is_packed_sbs_or_tab()))
        {
            StereoOutput::Mono2D
        } else {
            StereoOutput::Preserve
        }
    }

    /// Returns the ffmpeg filter chain required to downmix 3D video to 2D mono, if active.
    pub fn active_stereo_mono_filter(&self) -> Option<&'static str> {
        if self.effective_stereo_output() == StereoOutput::Mono2D {
            self.video_3d_format.and_then(stereo_mono_filter)
        } else {
            None
        }
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
        // B228 — `filler` is `#[serde(skip)]` (see its own doc comment for
        // why: unlike this field, ANY non-skip presence — even neutralised
        // to a constant here — would still shift every OTHER rendition's
        // hash), so it is already absent from `canon` below and needs no
        // clearing here. Noted for the same reason `burn_subtitle_stream_index`
        // above IS cleared explicitly: the next per-segment field added to
        // this struct should ask which of the two techniques it needs.
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
        if o.effective_stereo_output() == StereoOutput::Preserve {
            o.video_3d_format = None;
            o.stereo_output = StereoOutput::Preserve;
        } else {
            o.stereo_output = StereoOutput::Mono2D;
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
            source_video_kind: Default::default(),
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
            filler: None,
            video_3d_format: None,
            stereo_output: StereoOutput::Preserve,
        };
        assert_eq!(o.start_position_seconds(), Some(3.0));
        assert_eq!(o.duration_seconds(), Some(5.0));
    }

    #[test]
    fn zero_start_returns_none() {
        let o = TranscodeOptions {
            source_video_codec: None,
            source_video_kind: Default::default(),
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
            filler: None,
            video_3d_format: None,
            stereo_output: StereoOutput::Preserve,
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
            source_video_kind: Default::default(),
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
            filler: None,
            video_3d_format: None,
            stereo_output: StereoOutput::Preserve,
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
    fn only_plain_eight_bit_sdr_with_even_dimensions_is_plain() {
        use SourceVideoKind::*;
        let k = SourceVideoKind::classify;
        assert_eq!(
            k(Some("yuv420p"), Some("bt709"), Some(1920), Some(1080)),
            Plain8BitSdr
        );
        assert_eq!(
            k(Some("yuv420p"), None, Some(1280), Some(720)),
            Plain8BitSdr
        );
        assert_eq!(
            k(Some("yuv420p10le"), Some("bt709"), Some(1920), Some(1080)),
            Other
        );
        assert_eq!(k(Some("yuv444p"), None, Some(1920), Some(1080)), Other);
        assert_eq!(
            k(Some("yuv420p"), Some("smpte2084"), Some(3840), Some(2160)),
            Other
        );
        assert_eq!(
            k(
                Some("yuv420p"),
                Some("arib-std-b67"),
                Some(1920),
                Some(1080)
            ),
            Other
        );
        assert_eq!(k(Some("yuv420p"), None, Some(1919), Some(1080)), Other);
        assert_eq!(k(Some("yuv420p"), None, Some(1920), Some(1081)), Other);
    }

    #[test]
    fn a_missing_probe_field_is_unknown_never_plain() {
        use SourceVideoKind::Unknown;
        let k = SourceVideoKind::classify;
        assert_eq!(k(None, None, Some(1920), Some(1080)), Unknown);
        assert_eq!(k(Some("yuv420p"), None, None, Some(1080)), Unknown);
        assert_eq!(k(Some("yuv420p"), None, Some(1920), None), Unknown);
        assert_eq!(SourceVideoKind::default(), Unknown);
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
            source_video_kind: Default::default(),
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
            filler: None,
            video_3d_format: None,
            stereo_output: StereoOutput::Preserve,
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

    #[test]
    fn stereo_output_all_labels_distinct_and_non_empty() {
        let mut labels = std::collections::HashSet::new();
        for variant in StereoOutput::ALL {
            assert!(!variant.label().is_empty());
            assert_eq!(variant.label(), variant.as_str());
            assert!(labels.insert(variant.label()));
        }
    }

    #[test]
    fn stereo_mono_filters_for_all_formats() {
        assert_eq!(
            stereo_mono_filter(Video3DFormat::HalfSideBySide),
            Some("crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1")
        );
        assert_eq!(
            stereo_mono_filter(Video3DFormat::FullSideBySide),
            Some("crop=iw/2:ih:0:0,setsar=1")
        );
        assert_eq!(
            stereo_mono_filter(Video3DFormat::HalfTopAndBottom),
            Some("crop=iw:ih/2:0:0,scale=iw:ih*2,setsar=1")
        );
        assert_eq!(
            stereo_mono_filter(Video3DFormat::FullTopAndBottom),
            Some("crop=iw:ih/2:0:0,setsar=1")
        );
        assert_eq!(stereo_mono_filter(Video3DFormat::Mvc), None);
    }

    #[test]
    fn preserve_vs_mono2d_rendition_keys_distinct() {
        let path = std::path::Path::new("/m/3d.mkv");
        let mut preserve = cmaf_opts();
        preserve.video_3d_format = Some(Video3DFormat::HalfSideBySide);
        preserve.stereo_output = StereoOutput::Preserve;

        let mut mono = preserve.clone();
        mono.stereo_output = StereoOutput::Mono2D;

        assert_ne!(
            RenditionKey::new(path, &preserve),
            RenditionKey::new(path, &mono)
        );
    }

    #[test]
    fn effective_stereo_output_and_active_filter_burn_in() {
        let mut o = cmaf_opts();
        o.video_3d_format = Some(Video3DFormat::HalfSideBySide);
        o.stereo_output = StereoOutput::Preserve;

        // Without subtitle burn, stays Preserve and has no filter.
        assert_eq!(o.effective_stereo_output(), StereoOutput::Preserve);
        assert_eq!(o.active_stereo_mono_filter(), None);

        // Explicit Mono2D activates filter even without burn.
        o.stereo_output = StereoOutput::Mono2D;
        assert_eq!(o.effective_stereo_output(), StereoOutput::Mono2D);
        assert_eq!(
            o.active_stereo_mono_filter(),
            Some("crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1")
        );

        // Reset to Preserve: burn_subtitle_stream_index forces Mono2D on packed 3D (B230)
        o.stereo_output = StereoOutput::Preserve;
        o.burn_subtitle_stream_index = Some(2);
        assert_eq!(o.effective_stereo_output(), StereoOutput::Mono2D);
        assert_eq!(
            o.active_stereo_mono_filter(),
            Some("crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1")
        );

        // burn_intent also forces Mono2D
        o.burn_subtitle_stream_index = None;
        o.burn_intent = true;
        assert_eq!(o.effective_stereo_output(), StereoOutput::Mono2D);
        assert_eq!(
            o.active_stereo_mono_filter(),
            Some("crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1")
        );

        // MVC source is NOT packed SBS or TAB, so burn does not force Mono2D
        o.video_3d_format = Some(Video3DFormat::Mvc);
        assert_eq!(o.effective_stereo_output(), StereoOutput::Preserve);
        assert_eq!(o.active_stereo_mono_filter(), None);
    }

    #[test]
    fn bincode_and_json_dual_serialization_roundtrip() {
        let mut o = cmaf_opts();
        o.video_3d_format = Some(Video3DFormat::HalfSideBySide);
        o.stereo_output = StereoOutput::Mono2D;

        // Bincode roundtrip (binary, non-human-readable)
        let bytes = bincode::serialize(&o).unwrap();
        let decoded: TranscodeOptions = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.video_3d_format, Some(Video3DFormat::HalfSideBySide));
        assert_eq!(decoded.stereo_output, StereoOutput::Mono2D);

        // JSON serialization (human-readable) preserves fields when Mono2D
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"stereo_output\":\"mono2d\""));
        assert!(json.contains("\"video_3d_format\":"));

        // JSON serialization omits fields when None / Preserve
        let mut p = cmaf_opts();
        p.video_3d_format = None;
        p.stereo_output = StereoOutput::Preserve;
        let json_p = serde_json::to_string(&p).unwrap();
        assert!(!json_p.contains("video_3d_format"));
        assert!(!json_p.contains("stereo_output"));

        // Bincode roundtrip with None / Preserve
        let bytes_p = bincode::serialize(&p).unwrap();
        let decoded_p: TranscodeOptions = bincode::deserialize(&bytes_p).unwrap();
        assert_eq!(decoded_p.video_3d_format, None);
        assert_eq!(decoded_p.stereo_output, StereoOutput::Preserve);
    }
}
