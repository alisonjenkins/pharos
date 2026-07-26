//! Typed seek primitives shared by every video delivery path (T90).
//!
//! Seeking used to be inconsistent because each delivery handler
//! (`deliver_stream`/`serve_from_offset` for DirectPlay, `serve_segment` for
//! HLS, `vp9_segment` for VP9 fMP4) hand-built its own response, so nothing
//! forced a path to answer a seek with a decodable, self-consistent body and
//! every divergence was an independent per-handler slip. This module hoists the
//! load-bearing decisions into the type system so the bugs a cross-path audit
//! found are *unrepresentable* rather than caught (or not) in review:
//!
//! - [`ContentRange`] — a partial body is always `206` and always carries a
//!   known length (kills the actix-files `Range: bytes=0-` → `200` regression,
//!   B94, and the `>16 MiB` Content-Length strip → chunked 206).
//! - [`DeliveryMime`] — the source Content-Type is computed once, so GET-open,
//!   GET-seek and HEAD can never disagree (kills the mkv/VP9 → `video/webm`
//!   relabel living only in the NamedFile branch while seek/HEAD served
//!   `video/x-matroska`, which Firefox rejects).
//! - [`SegmentGrid`] / [`SegmentIndex`] — a segment index is constructible only
//!   in-bounds, so an over-index degrades to a typed `None` (→ 404/416) instead
//!   of the vp9 `NoMoov` → 500 or an h264 empty-tail cached 200.
//! - [`CutTolerance`] / [`ResyncWitness`] — a byte-range built from a *time*
//!   target is only constructible for a container that tolerates an interior
//!   cut, so shipping a headerless interior slice of an mp4/mkv/webm (an
//!   undecodable 206) will not compile.
//!
//! [`SeekableDelivery`] ties them together: every delivery path declares its
//! honest cut-tolerance and accuracy, so the router stops assuming a uniform
//! seek contract across paths that do not share one.

use actix_web::http::header::HeaderValue;
use actix_web::http::StatusCode;
use pharos_core::MediaItem;

// ─────────────────────────────── ContentRange ──────────────────────────────

/// A resolved, well-formed partial-content byte window over a source file.
///
/// Built only via [`ContentRange::from_offset`], which returns `None` when the
/// offset is at/after EOF (the caller answers `416`), so a past-EOF or inverted
/// window is unrepresentable. [`status`](Self::status) is hard-wired to `206`
/// and [`content_length`](Self::content_length) is always known — the two
/// DirectPlay wire bugs the audit found (a Range answered `200`; a large 206
/// with its Content-Length stripped, forcing chunked framing) cannot recur once
/// a seek response is built from this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRange {
    offset: u64,
    end: u64,
    total: u64,
}

impl ContentRange {
    /// A window `[offset, total)` served as `206`. `None` when `offset >= total`
    /// (→ the caller returns `416 Range Not Satisfiable`).
    pub fn from_offset(offset: u64, total: u64) -> Option<Self> {
        (offset < total).then_some(Self {
            offset,
            end: total - 1,
            total,
        })
    }

    /// Always `206 Partial Content`. There is no constructor that yields `200`.
    pub const fn status(&self) -> StatusCode {
        StatusCode::PARTIAL_CONTENT
    }

    /// The `Content-Range: bytes A-B/T` header value.
    pub fn header_value(&self) -> HeaderValue {
        // Only ASCII digits + "bytes -/" — always valid header bytes; the
        // fallback exists solely to satisfy `clippy::unwrap_used` and is
        // unreachable.
        HeaderValue::from_str(&format!(
            "bytes {}-{}/{}",
            self.offset, self.end, self.total
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("bytes 0-0/0"))
    }

    /// A capped window `[offset, min(offset + max_len, total))` served as `206`.
    ///
    /// Caps an over-large or open-ended client byte-range so a DirectPlay
    /// response is a bounded chunk the client re-requests, rather than one
    /// multi-GB body streamed to EOF. A single uncapped `bytes=X-` answer (up to
    /// ~1.8 GB for a feature film) makes a reverse proxy temp-file the whole
    /// remainder before relaying — so an unbuffered seek takes minutes and a
    /// stalled/reset progressive connection restarts the `<video>` at 0
    /// (Deadpool). Capping a range the client explicitly asked for is always
    /// valid regardless of container (unlike a StartTimeTicks time→byte cut,
    /// which needs a [`ResyncWitness`]): we return a prefix of the requested
    /// bytes and the client asks for the next window.
    ///
    /// `None` when `offset >= total` (→ `416`) or `max_len == 0`.
    pub fn window(offset: u64, total: u64, max_len: u64) -> Option<Self> {
        if offset >= total || max_len == 0 {
            return None;
        }
        let end = (offset + max_len - 1).min(total - 1);
        Some(Self { offset, end, total })
    }

    /// The mandatory `Content-Length` — the number of bytes in the window.
    /// End-based so a capped [`window`](Self::window) reports its true (shorter)
    /// length, not the whole tail; equals `total - offset` for a
    /// [`from_offset`](Self::from_offset) window whose end is EOF.
    pub const fn content_length(&self) -> u64 {
        self.end - self.offset + 1
    }

    /// The inclusive end byte of the window.
    pub const fn end(&self) -> u64 {
        self.end
    }

    /// Byte offset the body starts at.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Total file length.
    pub const fn total(&self) -> u64 {
        self.total
    }
}

// ──────────────────────────────── ByteRange ────────────────────────────────

/// A single HTTP byte-range request — `bytes=START-` or `bytes=START-END`.
///
/// Suffix ranges (`bytes=-N`) and multi-range requests (`bytes=a-b,c-d`) parse
/// to `None`; the caller defers those to `NamedFile`, which handles the full
/// range grammar. This exists only to recognise the one shape a `<video>` (or
/// native player) actually sends for progressive playback/seek so an
/// over-large window can be capped ([`ContentRange::window`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    /// First byte requested.
    pub start: u64,
    /// Inclusive last byte, or `None` for an open-ended `bytes=START-`.
    pub end: Option<u64>,
}

impl ByteRange {
    /// Parse a `Range` header value. `None` for anything but a single
    /// `bytes=START-` / `bytes=START-END` (suffix and multi-range included).
    pub fn parse(header: &str) -> Option<Self> {
        let spec = header.trim().strip_prefix("bytes=")?;
        if spec.contains(',') {
            return None; // multi-range → NamedFile
        }
        let (a, b) = spec.split_once('-')?;
        // A suffix range ("bytes=-N") has an empty start → parse fails → None.
        let start: u64 = a.trim().parse().ok()?;
        let end = {
            let b = b.trim();
            if b.is_empty() {
                None
            } else {
                Some(b.parse::<u64>().ok()?)
            }
        };
        if let Some(e) = end {
            if e < start {
                return None; // inverted → NamedFile answers 416
            }
        }
        Some(Self { start, end })
    }

    /// Bytes this range would serve from a `total`-byte file, or `None` when the
    /// start is at/after EOF (the caller answers `416`). An open end resolves to
    /// EOF.
    pub fn served_len(&self, total: u64) -> Option<u64> {
        if self.start >= total {
            return None;
        }
        let end = self.end.unwrap_or(total - 1).min(total - 1);
        Some(end - self.start + 1)
    }
}

// ─────────────────────────────── DeliveryMime ──────────────────────────────

/// The Content-Type a DirectPlay source is served as, computed once from its
/// (container, video codec) so GET-open, GET-seek and HEAD agree by
/// construction.
///
/// A Matroska/WebM file carrying only browser-legal codecs (VP8/VP9/AV1 +
/// Opus/Vorbis) is playable as `video/webm`, but `mime_guess` maps `.mkv` to
/// `video/x-matroska`, which Firefox rejects outright. The relabel used to live
/// only in `deliver_stream`'s NamedFile branch, so a StartTimeTicks seek
/// (`serve_from_offset`) and the HEAD probe (`head_response`) served the
/// rejected type and regressed a stream that plain-opened fine. One constructor,
/// shared by all three, removes the divergence.
#[derive(Clone, Debug)]
pub struct DeliveryMime(HeaderValue);

impl DeliveryMime {
    /// Compute the served Content-Type for `item`'s source file.
    pub fn for_source(item: &MediaItem) -> Self {
        if source_is_webm_legal(item) {
            return Self(HeaderValue::from_static("video/webm"));
        }
        let guessed = mime_guess::from_path(&item.path)
            .first_or_octet_stream()
            .to_string();
        Self(
            HeaderValue::from_str(&guessed)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        )
    }

    /// The `Content-Type` header value.
    pub fn header(&self) -> HeaderValue {
        self.0.clone()
    }
}

/// True when the source really IS a WebM container, and so may be served as
/// `video/webm`.
///
/// This used to relabel any Matroska carrying WebM-legal video (VP8/VP9/AV1),
/// on the premise that a browser would then accept it. It does not (B107):
/// a browser demuxes the container before it reaches a codec, and Firefox has
/// no Matroska demuxer — it reads the EBML DocType, finds `matroska` where
/// WebM requires `webm`, and refuses the file however WebM-legal its streams
/// are. The relabel therefore never made a .mkv playable; it only made pharos
/// describe it as something it is not, and Firefox reported the container it
/// had sniffed rather than the Content-Type we sent, which is why the error
/// named a type pharos never emitted.
///
/// The extension is the discriminator, because ffprobe reports `matroska,webm`
/// for every member of the family (see `container_for`).
fn source_is_webm_legal(item: &MediaItem) -> bool {
    let ext_is_webm = item
        .path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("webm"));
    // A probe reporting bare `webm` (no `matroska` alias) is unambiguous too.
    let probe_is_webm = item
        .probe
        .container
        .as_deref()
        .is_some_and(|c| c.eq_ignore_ascii_case("webm"));
    ext_is_webm || probe_is_webm
}

// ──────────────────────────── Cut tolerance ────────────────────────────────

/// Whether a container can be cut at an arbitrary *interior byte* and still
/// decode from there.
///
/// Header-prefixed / index-at-EOF containers (mp4/mkv/webm/mov: the moov, EBML
/// SeekHead + cues, or ftyp live at file start or EOF, not at an interior byte)
/// **cannot** — a raw slice from the middle is headerless and undecodable. Only
/// self-framing resync streams (MPEG-TS, ADTS-AAC, MP3) can. A [`ContentRange`]
/// built from a *time* target (StartTimeTicks) is gated on a [`ResyncWitness`],
/// so the "raw byte cut of an mp4/mkv" path — the highest-severity DirectPlay
/// seek bug — cannot be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutTolerance {
    /// mp4 / mkv / webm / mov / ogg / flac — index not at an interior byte.
    HeaderPrefixed,
    /// MPEG-TS / ADTS-AAC / MP3 — self-framing, resyncs from any packet.
    Resync,
}

impl CutTolerance {
    /// Classify from the probed container name and/or the file extension.
    /// Unknown ⇒ the conservative [`HeaderPrefixed`](Self::HeaderPrefixed): a
    /// time-seek on it is refused (routed to segmented delivery) rather than
    /// shipped as a possibly-undecodable slice.
    pub fn for_source(item: &MediaItem) -> Self {
        let container = item
            .probe
            .container
            .as_deref()
            .map(|c| c.to_ascii_lowercase());
        let ext = item
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let tag = |s: &str| {
            container.as_deref().is_some_and(|c| c.contains(s)) || ext.as_deref() == Some(s)
        };
        // MPEG-TS + raw elementary self-framing audio resync from any packet.
        if tag("mpegts") || tag("ts") || tag("aac") || tag("adts") || tag("mp3") || tag("mpeg") {
            CutTolerance::Resync
        } else {
            CutTolerance::HeaderPrefixed
        }
    }
}

/// Zero-sized proof that a container tolerates an interior byte cut. Obtainable
/// only from [`CutTolerance::Resync`], so a [`ContentRange`] answering a *time*
/// seek can be built only for a resync container — the mp4/mkv headerless-slice
/// code path does not compile.
#[derive(Clone, Copy, Debug)]
pub struct ResyncWitness(());

impl ResyncWitness {
    /// `Some` iff `tolerance` is [`CutTolerance::Resync`].
    pub fn of(tolerance: CutTolerance) -> Option<Self> {
        matches!(tolerance, CutTolerance::Resync).then_some(Self(()))
    }
}

// ──────────────────────────── SegmentGrid ──────────────────────────────────

// The segment grid itself lives in `pharos_core::segment_grid`, below both this
// crate and `pharos-cache`. It was defined here, in the HTTP layer, where the
// cache could not reach it — so the continuous-audio rendition grew a private
// copy that rounded differently, and two sessions wrote different bytes under
// one segment filename. Re-exported rather than wrapped so every call site
// keeps naming it through `seek::`.
pub use pharos_core::{
    frame_snapped_start, segment_range, segment_seek_bias, SegmentGrid, SegmentIndex,
    SEGMENT_SECONDS,
};

// ──────────────────────────── SeekableDelivery ─────────────────────────────

/// The honest seek accuracy a delivery path offers — so the router stops
/// assuming every path is byte-exact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeekAccuracy {
    /// Client-driven HTTP Range against its own container index — byte/frame
    /// exact (DirectPlay in a browser).
    ByteExact,
    /// Fixed segment grid, keyframe-clean at boundaries (all HLS/VP9 paths).
    SegmentGrid { seg_secs: f64 },
}

/// Implemented by every video delivery path so the compiler records — and the
/// router can query — how that path honours a seek. Deliberately small: it
/// carries the two facts a path must not lie about (can it be interior-cut, and
/// how accurate is its seek), which is what makes cross-path seeking consistent.
pub trait SeekableDelivery {
    /// Whether this path may serve a byte-range cut from an interior offset.
    fn cut_tolerance(&self) -> CutTolerance;

    /// The seek granularity/accuracy this path actually provides.
    fn accuracy(&self) -> SeekAccuracy;
}

/// DirectPlay progressive download (raw source over HTTP byte ranges).
pub struct DirectPlayDelivery<'a> {
    pub item: &'a MediaItem,
}

impl SeekableDelivery for DirectPlayDelivery<'_> {
    fn cut_tolerance(&self) -> CutTolerance {
        CutTolerance::for_source(self.item)
    }
    fn accuracy(&self) -> SeekAccuracy {
        SeekAccuracy::ByteExact
    }
}

/// HLS on-demand transcode (`.ts` H.264/HEVC ladder) and VP9 fMP4 fragments —
/// both seek by segment index on the same grid.
pub struct SegmentDelivery;

impl SeekableDelivery for SegmentDelivery {
    fn cut_tolerance(&self) -> CutTolerance {
        // Never serves a byte-range; whole self-contained segments only.
        CutTolerance::HeaderPrefixed
    }
    fn accuracy(&self) -> SeekAccuracy {
        SeekAccuracy::SegmentGrid {
            seg_secs: SEGMENT_SECONDS,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::{MediaItem, MediaKind, MediaProbe};

    fn item(container: Option<&str>, video_codec: Option<&str>, path: &str) -> MediaItem {
        MediaItem {
            id: 1,
            path: path.into(),
            title: "t".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                container: container.map(String::from),
                video_codec: video_codec.map(String::from),
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
        }
    }

    #[test]
    fn content_range_past_eof_is_unrepresentable() {
        // offset == total and offset > total both yield None → caller sends 416.
        assert!(ContentRange::from_offset(100, 100).is_none());
        assert!(ContentRange::from_offset(200, 100).is_none());
    }

    #[test]
    fn content_range_is_always_206_with_known_length() {
        let cr = ContentRange::from_offset(10, 100).unwrap();
        assert_eq!(cr.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(cr.content_length(), 90);
        assert_eq!(cr.offset(), 10);
        assert_eq!(cr.total(), 100);
        assert_eq!(cr.header_value().to_str().unwrap(), "bytes 10-99/100");
    }

    #[test]
    fn content_range_whole_file_still_206() {
        // The B94 case: a Range spanning the whole file is partial by
        // definition and must never be a 200.
        let cr = ContentRange::from_offset(0, 100).unwrap();
        assert_eq!(cr.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(cr.header_value().to_str().unwrap(), "bytes 0-99/100");
    }

    #[test]
    fn content_range_window_caps_to_max_len() {
        // A window well inside the file is exactly max_len bytes.
        let cr = ContentRange::window(10, 1000, 100).unwrap();
        assert_eq!(cr.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(cr.offset(), 10);
        assert_eq!(cr.end(), 109);
        assert_eq!(cr.content_length(), 100);
        assert_eq!(cr.header_value().to_str().unwrap(), "bytes 10-109/1000");
    }

    #[test]
    fn content_range_window_clamps_to_eof() {
        // Near EOF the window is shorter than max_len (never past the last byte).
        let cr = ContentRange::window(950, 1000, 100).unwrap();
        assert_eq!(cr.end(), 999);
        assert_eq!(cr.content_length(), 50);
        assert_eq!(cr.header_value().to_str().unwrap(), "bytes 950-999/1000");
    }

    #[test]
    fn content_range_window_rejects_past_eof_and_zero_len() {
        assert!(ContentRange::window(1000, 1000, 100).is_none()); // at EOF
        assert!(ContentRange::window(1001, 1000, 100).is_none()); // past EOF
        assert!(ContentRange::window(10, 1000, 0).is_none()); // zero cap
    }

    #[test]
    fn byte_range_parses_open_and_closed() {
        assert_eq!(
            ByteRange::parse("bytes=100-"),
            Some(ByteRange {
                start: 100,
                end: None
            })
        );
        assert_eq!(
            ByteRange::parse("bytes=100-199"),
            Some(ByteRange {
                start: 100,
                end: Some(199)
            })
        );
        assert_eq!(
            ByteRange::parse("bytes=0-"),
            Some(ByteRange {
                start: 0,
                end: None
            })
        );
    }

    #[test]
    fn byte_range_rejects_suffix_multi_and_garbage() {
        assert_eq!(ByteRange::parse("bytes=-500"), None); // suffix → NamedFile
        assert_eq!(ByteRange::parse("bytes=0-99,200-299"), None); // multi-range
        assert_eq!(ByteRange::parse("bytes=200-100"), None); // inverted
        assert_eq!(ByteRange::parse("bytes=abc"), None);
        assert_eq!(ByteRange::parse("items=0-5"), None); // wrong unit
    }

    #[test]
    fn byte_range_served_len() {
        // Open-ended resolves to EOF.
        assert_eq!(
            ByteRange {
                start: 100,
                end: None
            }
            .served_len(1000),
            Some(900)
        );
        // Closed range is end-inclusive.
        assert_eq!(
            ByteRange {
                start: 100,
                end: Some(199)
            }
            .served_len(1000),
            Some(100)
        );
        // Start at/after EOF → None (→ 416).
        assert_eq!(
            ByteRange {
                start: 1000,
                end: None
            }
            .served_len(1000),
            None
        );
    }

    #[test]
    fn delivery_mime_never_calls_a_matroska_webm() {
        // B107 — a vp9/av1 .mkv is NOT a WebM file. Serving it as `video/webm`
        // did not make Firefox play it (no Matroska demuxer, DocType checked),
        // it only misdescribed the bytes.
        let mkv_vp9 = item(Some("matroska,webm"), Some("vp9"), "/m/x.mkv");
        assert_eq!(
            DeliveryMime::for_source(&mkv_vp9)
                .header()
                .to_str()
                .unwrap(),
            "video/x-matroska"
        );
    }

    #[test]
    fn delivery_mime_serves_a_genuine_webm_as_webm() {
        // Same ffprobe alias list, real WebM file — the extension decides.
        let webm_av1 = item(Some("matroska,webm"), Some("av1"), "/m/x.webm");
        assert_eq!(
            DeliveryMime::for_source(&webm_av1)
                .header()
                .to_str()
                .unwrap(),
            "video/webm"
        );
    }

    #[test]
    fn delivery_mime_leaves_mp4_and_h264_mkv_alone() {
        let mp4 = item(Some("mp4"), Some("h264"), "/m/x.mp4");
        assert_eq!(
            DeliveryMime::for_source(&mp4).header().to_str().unwrap(),
            "video/mp4"
        );
        let mkv_h264 = item(Some("matroska"), Some("h264"), "/m/x.mkv");
        assert_eq!(
            DeliveryMime::for_source(&mkv_h264)
                .header()
                .to_str()
                .unwrap(),
            "video/x-matroska"
        );
    }

    #[test]
    fn cut_tolerance_gates_the_headerless_slice_bug() {
        // mp4/mkv/webm CANNOT be interior-cut → no ResyncWitness → a
        // StartTimeTicks byte-range on them is unrepresentable.
        for (c, e) in [
            ("mp4", "/x.mp4"),
            ("matroska", "/x.mkv"),
            ("webm", "/x.webm"),
        ] {
            let it = item(Some(c), Some("h264"), e);
            assert_eq!(CutTolerance::for_source(&it), CutTolerance::HeaderPrefixed);
            assert!(ResyncWitness::of(CutTolerance::for_source(&it)).is_none());
        }
        // MPEG-TS / ADTS / MP3 resync from any packet → witness available.
        for (c, e) in [("mpegts", "/x.ts"), ("aac", "/x.aac"), ("mp3", "/x.mp3")] {
            let it = item(Some(c), None, e);
            assert_eq!(CutTolerance::for_source(&it), CutTolerance::Resync);
            assert!(ResyncWitness::of(CutTolerance::for_source(&it)).is_some());
        }
    }

    #[test]
    fn unknown_container_is_conservatively_header_prefixed() {
        let it = item(None, Some("h264"), "/x.unknownext");
        assert_eq!(CutTolerance::for_source(&it), CutTolerance::HeaderPrefixed);
    }
    #[test]
    fn directplay_declares_byte_exact_and_source_cut_tolerance() {
        let mp4 = item(Some("mp4"), Some("h264"), "/x.mp4");
        let d = DirectPlayDelivery { item: &mp4 };
        assert_eq!(d.accuracy(), SeekAccuracy::ByteExact);
        assert_eq!(d.cut_tolerance(), CutTolerance::HeaderPrefixed);
    }

    #[test]
    fn segment_delivery_declares_grid_accuracy() {
        let d = SegmentDelivery;
        assert_eq!(d.accuracy(), SeekAccuracy::SegmentGrid { seg_secs: 6.0 });
    }
}
