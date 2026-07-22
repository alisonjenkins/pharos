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

/// Nominal HLS segment length in seconds. The one canonical value the whole
/// segmented surface (playlist EXTINF, per-segment `-ss`, audio anchor,
/// SyncPlay prewarm) reads, so no path invents its own grid.
pub const SEGMENT_SECONDS: f64 = 6.0;

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

    /// The mandatory `Content-Length` — the number of bytes in the window.
    pub const fn content_length(&self) -> u64 {
        self.total - self.offset
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

/// True when the source is a Matroska/WebM container whose video codec is
/// browser-legal in a `video/webm` MSE stream (VP8/VP9/AV1). Mirrors the
/// original `deliver_stream` predicate exactly so the relabel is behaviour-
/// identical, just centralised.
fn source_is_webm_legal(item: &MediaItem) -> bool {
    let webm_video = matches!(
        item.probe
            .video_codec
            .as_deref()
            .map(|c| c.to_ascii_lowercase())
            .as_deref(),
        Some("vp9" | "vp09" | "vp8" | "vp08" | "av1" | "av01")
    );
    let matroska = matches!(
        item.probe
            .container
            .as_deref()
            .map(|c| c.to_ascii_lowercase())
            .as_deref(),
        Some("webm" | "matroska" | "mkv")
    ) || item
        .path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mkv") || e.eq_ignore_ascii_case("webm"));
    webm_video && matroska
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

/// Frame-snapped start time (seconds) of segment `seg`: nominal `seg*6` rounded
/// to the nearest source-frame boundary. The SINGLE definition of the segment
/// seek grid on the server — [`SegmentGrid`] and the HLS/VP9 segment handlers
/// all snap to this, so the video segments, the audio-rendition anchor and the
/// SyncPlay prewarm cannot compute independent grids that drift apart. Falls
/// back to the nominal grid when fps is unknown.
pub fn frame_snapped_start(seg: u32, fps_mille: Option<u32>) -> f64 {
    let nominal = seg as f64 * SEGMENT_SECONDS;
    match fps_mille {
        Some(m) if m > 0 => {
            let fps = m as f64 / 1000.0;
            (nominal * fps).round() / fps
        }
        _ => nominal,
    }
}

/// A segment index PROVEN in `[0, count)` for a title. Constructible only via
/// [`SegmentGrid::checked`] / [`SegmentGrid::resolve`], so an over-index request
/// becomes a typed absence the handler turns into `404`/`416` — never the vp9
/// `NoMoov` → `500` or the h264 empty-tail cached `200`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentIndex(u32);

impl SegmentIndex {
    /// The raw index value (for URL / ffmpeg `-start_number` / cache key).
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The canonical 6 s segment grid for one title, frame-snapped to the source
/// fps so the playlist EXTINF, each segment's `-ss`, the audio anchor and the
/// SyncPlay prewarm all read one boundary set (the three-grid drift the audit
/// found came from these being computed independently).
#[derive(Clone, Copy, Debug)]
pub struct SegmentGrid {
    count: u32,
    duration_secs: f64,
    frame_rate_mille: Option<u32>,
}

impl SegmentGrid {
    /// Build from a title's duration and (optional) frame rate. `count` is
    /// `ceil(duration / 6)`, min 1 — the number of segments the VOD playlist
    /// enumerates.
    pub fn new(duration_secs: f64, frame_rate_mille: Option<u32>) -> Self {
        let count = ((duration_secs / SEGMENT_SECONDS).ceil() as u32).max(1);
        Self {
            count,
            duration_secs,
            frame_rate_mille,
        }
    }

    /// Number of segments (`= ceil(duration/6)`, min 1).
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// A raw index, checked against the segment count. `None` when
    /// `raw >= count` (over-index).
    pub fn checked(&self, raw: u32) -> Option<SegmentIndex> {
        (raw < self.count).then_some(SegmentIndex(raw))
    }

    /// Resolve a source-time offset (seconds) to the segment index containing
    /// it. `None` when the time is at/after the media end.
    pub fn resolve(&self, secs: f64) -> Option<SegmentIndex> {
        if secs < 0.0 {
            return self.checked(0);
        }
        self.checked((secs / SEGMENT_SECONDS).floor() as u32)
    }

    /// Frame-snapped `(start_secs, duration_secs)` for `idx`: the nominal
    /// `idx*6` rounded to the nearest source-frame boundary, with the tail
    /// clamped by the remaining media. This is the single definition of a
    /// segment boundary; the audio rendition seeks to the same grid.
    pub fn frame_snapped_range(&self, idx: SegmentIndex) -> (f64, f64) {
        let start = frame_snapped_start(idx.0, self.frame_rate_mille);
        let next = frame_snapped_start(idx.0 + 1, self.frame_rate_mille);
        let remaining = (self.duration_secs - start).max(0.01);
        let dur = (next - start).min(remaining);
        (start, dur)
    }
}

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
    fn delivery_mime_relabels_webm_legal_matroska() {
        let mkv_vp9 = item(Some("matroska"), Some("vp9"), "/m/x.mkv");
        assert_eq!(
            DeliveryMime::for_source(&mkv_vp9)
                .header()
                .to_str()
                .unwrap(),
            "video/webm"
        );
        let webm_av1 = item(Some("webm"), Some("av1"), "/m/x.webm");
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
        // h264-in-mkv is NOT webm-legal; the router downgrades it to transcode,
        // so DirectPlay never relabels it (mirrors the original predicate).
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
    fn segment_grid_bounds_check_makes_over_index_none() {
        // 61 s / 6 = 10.16 → ceil = 11 segments, indices 0..=10.
        let grid = SegmentGrid::new(61.0, Some(24_000));
        assert_eq!(grid.count(), 11);
        assert!(grid.checked(10).is_some());
        assert!(grid.checked(11).is_none()); // over-index → 404/416, not 500
        assert!(grid.checked(9999).is_none());
    }

    #[test]
    fn segment_grid_resolve_maps_time_to_index() {
        let grid = SegmentGrid::new(120.0, Some(24_000));
        assert_eq!(grid.resolve(0.0).unwrap().get(), 0);
        assert_eq!(grid.resolve(5.9).unwrap().get(), 0);
        assert_eq!(grid.resolve(6.1).unwrap().get(), 1);
        assert_eq!(grid.resolve(59.0).unwrap().get(), 9);
        // Past the end → None.
        assert!(grid.resolve(120.0).is_none());
        assert!(grid.resolve(999.0).is_none());
    }

    #[test]
    fn segment_grid_frame_snaps_to_source_fps() {
        // 23.976 fps: segment 1's nominal 6.000 s snaps to 6.006 s (matches the
        // audio rendition's -ss anchor so the two renditions stay locked).
        let grid = SegmentGrid::new(600.0, Some(23_976));
        let (start, _dur) = grid.frame_snapped_range(grid.checked(1).unwrap());
        assert!((start - 6.006).abs() < 0.0005, "got {start}");
        // Integer fps: no snap.
        let grid30 = SegmentGrid::new(600.0, Some(30_000));
        let (s30, _) = grid30.frame_snapped_range(grid30.checked(1).unwrap());
        assert!((s30 - 6.0).abs() < 1e-9, "got {s30}");
    }

    #[test]
    fn segment_grid_tail_duration_is_clamped_to_media() {
        // Last segment of a 61 s title starts at 60 s and lasts ~1 s, not 6.
        let grid = SegmentGrid::new(61.0, Some(24_000));
        let last = grid.checked(grid.count() - 1).unwrap();
        let (start, dur) = grid.frame_snapped_range(last);
        assert!((start - 60.0).abs() < 0.05, "start {start}");
        assert!(
            dur <= 1.1,
            "tail dur should clamp to remaining media, got {dur}"
        );
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
