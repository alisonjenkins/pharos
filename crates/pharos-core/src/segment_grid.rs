//! The canonical HLS segment grid.
//!
//! Every segmented delivery path — the mpegts ladder, the h264/VP9 fMP4
//! surfaces, the continuous-audio rendition, the SyncPlay prewarm and each
//! playlist builder — reads its boundaries from here and nowhere else.
//!
//! This lives in `pharos-core`, below both `pharos-cache` and `pharos-server`,
//! for a specific reason: it used to live in the server's HTTP layer, where the
//! cache could not reach it, and the continuous-audio rendition consequently
//! grew a private copy that rounded differently. Two sessions then wrote
//! different bytes under one segment filename. A grid that is only reachable
//! from one crate is a grid that the other crate will reimplement.

use crate::FrameRate;

/// Nominal HLS segment length in seconds. The one canonical value the whole
/// segmented surface (playlist EXTINF, per-segment `-ss`, audio anchor,
/// SyncPlay prewarm) reads, so no path invents its own grid.
pub const SEGMENT_SECONDS: f64 = 6.0;

// ──────────────────────────── SegmentGrid ──────────────────────────────────

/// Frame-snapped start time (seconds) of segment `seg`: the nominal `seg*6`
/// boundary moved onto the nearest real source frame. The SINGLE definition of
/// the segment seek grid on the server — [`SegmentGrid`] and the HLS/VP9
/// segment handlers all snap to this, so the video segments, the
/// audio-rendition anchor and the SyncPlay prewarm cannot compute independent
/// grids that drift apart.
///
/// The boundary is computed as a FRAME INDEX and converted back through
/// [`FrameRate::secs_of_frame`], so consecutive boundaries differ by a whole
/// number of frames *by construction*. That is the property that makes segment
/// N end exactly where N+1 begins; computing the boundary as `round(t*fps)/fps`
/// against a decimal fps instead leaves a sub-frame residue that the encoder
/// resolves by duplicating or dropping the boundary frame — a per-segment
/// stutter on every client.
///
/// `rate` is `None` only when the source has no usable frame rate (see
/// [`FrameRate`], which rejects the MPEG-TS 90 kHz container clock rather than
/// letting it masquerade as one). There is no frame grid to snap to in that
/// case, so the nominal grid is the honest answer — callers that care log it,
/// and the software encoders additionally receive `-enc_time_base` so the
/// boundary frame is still placed exactly.
pub fn frame_snapped_start(seg: u32, rate: Option<FrameRate>) -> f64 {
    let nominal = seg as f64 * SEGMENT_SECONDS;
    match rate {
        Some(r) => r.secs_of_frame(r.frame_index_at(nominal)),
        None => nominal,
    }
}

/// How far BEFORE its frame boundary a segment is seeked, in seconds: half a
/// frame (zero when the frame rate is unknown, since there is no frame to
/// measure).
///
/// A source frame can sit microseconds below a segment boundary — a real
/// measured case: the boundary is 12.012000 and the frame is at 12.011997.
/// Seeking segment N+1 to exactly the boundary then excludes that frame (its
/// pts is below the seek point) while segment N's `-t` also excludes it (it
/// falls on the cut). The frame belongs to neither segment and is silently
/// DROPPED — a visible hitch at almost every boundary, which is what made
/// certain titles stutter continuously.
///
/// Biasing the seek half a frame earlier makes the claim unambiguous: the
/// boundary frame is always the first frame of segment N+1, and never also the
/// last frame of segment N (which now ends half a frame early too). Every
/// segment shifts by the same amount, so consecutive segments still tile the
/// timeline exactly — verified against the real source: the previously dropped
/// frames reappear and every boundary joins to within 3 µs.
pub fn segment_seek_bias(rate: Option<FrameRate>) -> f64 {
    rate.map_or(0.0, |r| r.frame_duration_secs() / 2.0)
}

/// The canonical `(start, duration)` a segment is BOTH encoded with and
/// advertised as: the frame-snapped boundaries, each pulled back by
/// [`segment_seek_bias`] so no frame can fall between two segments. Start is
/// clamped at 0 for segment 0. One definition, so the playlist and the encoder
/// cannot describe different windows.
pub fn segment_range(seg: u32, rate: Option<FrameRate>) -> (f64, f64) {
    let bias = segment_seek_bias(rate);
    let start = (frame_snapped_start(seg, rate) - bias).max(0.0);
    let end = (frame_snapped_start(seg + 1, rate) - bias).max(0.0);
    (start, (end - start).max(0.001))
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
    /// Validated source frame rate; `None` when the source has none usable.
    rate: Option<FrameRate>,
}

impl SegmentGrid {
    /// Build from a title's duration and the source's `frame_rate_mille`
    /// scalar. The scalar is validated into a [`FrameRate`] here — an
    /// implausible one (the MPEG-TS 90 kHz container clock) becomes `None`
    /// rather than a rate that silently flattens the grid. `count` is
    /// `ceil(duration / 6)`, min 1 — the number of segments the VOD playlist
    /// enumerates.
    pub fn new(duration_secs: f64, frame_rate_mille: Option<u32>) -> Self {
        Self::with_rate(
            duration_secs,
            frame_rate_mille.and_then(FrameRate::from_mille),
        )
    }

    /// Build from an already-validated [`FrameRate`].
    pub fn with_rate(duration_secs: f64, rate: Option<FrameRate>) -> Self {
        let count = ((duration_secs / SEGMENT_SECONDS).ceil() as u32).max(1);
        Self {
            count,
            duration_secs,
            rate,
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
        let (start, dur) = segment_range(idx.0, self.rate);
        let remaining = (self.duration_secs - start).max(0.01);
        (start, dur.min(remaining))
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
        // 23.976 fps: segment 1's nominal 6.000 s snaps to the 6.006 s frame
        // boundary (matching the audio rendition's anchor so the two renditions
        // stay locked), then the seek is pulled back half a frame so a frame
        // sitting microseconds under the boundary cannot fall between segments.
        let half_frame = (1001.0 / 24_000.0) / 2.0;
        let grid = SegmentGrid::new(600.0, Some(23_976));
        let (start, _dur) = grid.frame_snapped_range(grid.checked(1).unwrap());
        assert!((start - (6.006 - half_frame)).abs() < 0.0005, "got {start}");
        // Integer fps: the boundary is already 6.0, still seeked half a frame
        // early for the same reason.
        let grid30 = SegmentGrid::new(600.0, Some(30_000));
        let (s30, _) = grid30.frame_snapped_range(grid30.checked(1).unwrap());
        assert!((s30 - (6.0 - 1.0 / 60.0)).abs() < 1e-9, "got {s30}");
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
}
