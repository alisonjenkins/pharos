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

/// The slice of the shared timeline one segment occupies.
///
/// Fields are private and the only constructor takes a segment INDEX, so a
/// caller cannot state a start position of its own. That is the point: the
/// segment grid was re-derived in five places and a frame fell between
/// segments at nearly every boundary, and separately a playlist's advertised
/// durations drifted from the transcoder's actual cut points. Both were one
/// path computing a timeline the others disagreed with. A path can no longer
/// hold a start position it did not get from the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentWindow {
    start_ticks: u64,
    duration_ticks: u64,
    /// The frame rate this window was snapped to.
    ///
    /// Kept, not discarded, so a layer holding only the window can say how many
    /// frames the window IMPLIES. Without it the segment completeness check had
    /// no honest way to know the source rate, so it could only test the gross
    /// "reached 90% of the requested duration" bound — which passes a segment
    /// missing up to 600 ms of video, i.e. a plainly visible stutter.
    ///
    /// `default` for wire compatibility: this type crosses the worker IPC
    /// boundary, and an older peer's payload simply carries no rate.
    #[serde(default)]
    rate: Option<crate::FrameRate>,
}

impl SegmentWindow {
    /// The window segment `index` occupies, frame-snapped to `rate` and
    /// CLAMPED by the media remaining after its start.
    ///
    /// The clamp is load-bearing. A title's final segment is shorter than a
    /// full 6 s, the playlist advertises that shortened EXTINF, and the
    /// encoder stops at end-of-file regardless — so an unclamped window there
    /// describes something the source cannot produce. Anything comparing what
    /// was produced against what was asked for then reads the last segment of
    /// every title as truncated, which is exactly what the completeness check
    /// did: it rejected the tail of a 1372 s title and returned 500 rather
    /// than serving it.
    ///
    /// Routed through [`SegmentGrid::frame_snapped_range`] so there is ONE
    /// clamp, shared with the playlist that advertises it.
    ///
    /// `total_duration_secs` is an `Option` because a source's duration is
    /// genuinely unknown until it is probed, and there is nothing to clamp
    /// against then. It must not be spelled `0.0`: the grid floors its
    /// remaining media at 0.01 s, so a zero total would hand back a 10 ms
    /// window for EVERY index, not just the tail — the encoder produces 10 ms
    /// of video per segment and playback never advances. A non-positive
    /// duration is treated as the same absence for that reason.
    pub fn for_segment(
        index: u32,
        rate: Option<FrameRate>,
        total_duration_secs: Option<f64>,
    ) -> Self {
        let (start, dur) = match total_duration_secs.filter(|t| *t > 0.0) {
            Some(total) => {
                let grid = SegmentGrid::with_rate(total, rate);
                match grid.checked(index) {
                    Some(i) => grid.frame_snapped_range(i),
                    // Past the end of the grid: the bounds check upstream turns
                    // this into a 404, so describe the nominal window rather
                    // than invent a clamped one.
                    None => segment_range(index, rate),
                }
            }
            // No probed duration: the nominal grid is the honest answer, and
            // the encoder stopping at EOF is the only clamp available.
            None => segment_range(index, rate),
        };
        Self {
            start_ticks: crate::time::Ticks::from_seconds(start).0,
            duration_ticks: crate::time::Ticks::from_seconds(dur).0,
            rate,
        }
    }

    /// The frame rate this window was snapped to, when the source had a usable
    /// one.
    pub fn rate(self) -> Option<crate::FrameRate> {
        self.rate
    }

    /// How many video frames this window implies, when the rate is known.
    ///
    /// Counts the frames whose presentation time falls in `[start, end)`, NOT
    /// `duration * fps` — the latter drifts for long titles, which is the whole
    /// reason `FrameRate` keeps the rational.
    ///
    /// Uses `ceil`, not `round`. The window's bounds carry the half-frame seek
    /// bias, so both ends land on EXACTLY `n + 0.5` frames and `round` would be
    /// deciding a frame's ownership on the last bit of a float. `ceil` asks the
    /// question that actually matters — "the first frame at or after this
    /// instant" — and is stable against that error either way.
    ///
    /// Segments therefore alternate 143/144 frames at 24000/1001, because the
    /// grid is a 6.000 s nominal snapped to frames, not a 6.006 s stride. The
    /// invariant is that adjacent windows TILE, not that each holds the same
    /// count.
    ///
    /// `None` when the source rate is unknown, because a guessed expectation is
    /// worse than none: it would make every segment of an unprobed source read
    /// as short.
    pub fn expected_frames(self) -> Option<u64> {
        let rate = self.rate?;
        let first = Self::first_frame_at_or_after(rate, self.start_seconds());
        let end =
            Self::first_frame_at_or_after(rate, self.start_seconds() + self.duration_seconds());
        Some(end.saturating_sub(first))
    }

    /// Index of the first frame presented at or after `secs`.
    fn first_frame_at_or_after(rate: crate::FrameRate, secs: f64) -> u64 {
        if !secs.is_finite() || secs <= 0.0 {
            return 0;
        }
        let (num, den) = rate.as_ratio();
        let scaled = secs * f64::from(num) / f64::from(den);
        if scaled >= u64::MAX as f64 {
            return u64::MAX;
        }
        scaled.ceil() as u64
    }

    pub fn start_ticks(self) -> u64 {
        self.start_ticks
    }

    pub fn duration_ticks(self) -> u64 {
        self.duration_ticks
    }

    pub fn start_seconds(self) -> f64 {
        crate::time::Ticks(self.start_ticks).seconds()
    }

    pub fn duration_seconds(self) -> f64 {
        crate::time::Ticks(self.duration_ticks).seconds()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod window_tests {
    use super::*;

    #[test]
    fn a_window_knows_how_many_frames_it_implies() {
        // The rate is what makes a shortfall measurable in FRAMES. Without it
        // the completeness check can only ask whether the encoder reached 90%
        // of the duration, which passes a segment missing ~14 frames at 23.976
        // — a visible stutter that reads as a healthy segment.
        let rate = FrameRate::from_mille(23_976);
        let total = 1372.121;
        let w = SegmentWindow::for_segment(3, rate, Some(total));
        let want = w.expected_frames().expect("rate known → frames known");
        // The grid is a 6.000 s nominal snapped to frames, not a 6.006 s
        // stride, so segments alternate 143/144 at 24000/1001. Both are
        // correct; what must hold is that they tile.
        assert!(
            (143..=144).contains(&want),
            "window {:?} implied {want} frames",
            (w.start_seconds(), w.duration_seconds())
        );

        // The real invariant: every window from 0..n covers a contiguous run of
        // frames with none dropped and none repeated. A frame lost at a
        // boundary IS the stutter, so this is the property worth asserting.
        let mut next = SegmentWindow::for_segment(0, rate, Some(total))
            .expected_frames()
            .unwrap();
        let mut cursor = SegmentWindow::for_segment(0, rate, Some(total));
        // Compared in TICKS, not seconds: a window stores its bounds as ticks,
        // so a start and its predecessor's end can differ by the last
        // representable unit (100 ns — 400 000× shorter than a frame). That is
        // the type's resolution, not a gap, and integers say so without float
        // fuzz. The frame accounting below is what proves nothing falls in it.
        for seg in 1..40u32 {
            let w = SegmentWindow::for_segment(seg, rate, Some(total));
            let prev_end = cursor.start_ticks() + cursor.duration_ticks();
            assert!(
                prev_end.abs_diff(w.start_ticks()) <= 1,
                "segment {seg} does not abut its predecessor: {prev_end} vs {}",
                w.start_ticks()
            );
            next += w.expected_frames().unwrap();
            cursor = w;
        }
        // Frames accumulated across 40 tiled windows must equal the frames the
        // whole span holds — the sum cannot hide a gap or an overlap.
        let r = rate.expect("rate");
        let span_end = cursor.start_seconds() + cursor.duration_seconds();
        let total_frames = SegmentWindow::first_frame_at_or_after(r, span_end)
            - SegmentWindow::first_frame_at_or_after(r, 0.0);
        assert_eq!(
            next, total_frames,
            "tiled windows must account for every frame exactly once"
        );
    }

    #[test]
    fn a_window_with_no_known_rate_implies_no_frame_count() {
        // A guessed expectation is worse than none: it would make every segment
        // of an unprobed source read as short.
        let w = SegmentWindow::for_segment(3, None, Some(600.0));
        assert_eq!(w.expected_frames(), None);
        assert_eq!(w.rate(), None);
    }

    #[test]
    fn the_final_segments_window_stops_at_the_end_of_the_media() {
        // A completeness check compares what ffmpeg produced against what the
        // window asked for. An unclamped tail asks for a full 6 s the source
        // cannot supply, so the last segment of nearly every title reads as
        // truncated — measured in production as a 500 on the final segment of
        // a 1372 s title.
        let total = 1372.121;
        let rate = FrameRate::from_mille(23_976);
        let grid = SegmentGrid::with_rate(total, rate);
        let last = grid.count() - 1;

        let w = SegmentWindow::for_segment(last, rate, Some(total));
        assert!(
            w.start_seconds() + w.duration_seconds() <= total + 0.01,
            "tail window runs past the media: {} + {} > {total}",
            w.start_seconds(),
            w.duration_seconds()
        );
        assert!(
            w.duration_seconds() < SEGMENT_SECONDS,
            "the tail is shorter than a full segment, got {}",
            w.duration_seconds()
        );

        // And it agrees with what the playlist advertises for that segment —
        // one clamp, not two.
        let (_, playlist_dur) = grid.frame_snapped_range(grid.checked(last).unwrap());
        assert!((w.duration_seconds() - playlist_dur).abs() < 1e-6);

        // A mid-file segment is untouched by the clamp.
        let mid = SegmentWindow::for_segment(10, rate, Some(total));
        assert!((mid.duration_seconds() - SEGMENT_SECONDS).abs() < 0.05);
    }

    #[test]
    fn an_unprobed_duration_does_not_clamp_every_window_to_a_sliver() {
        // The grid floors its remaining media at 0.01 s, so clamping against a
        // duration of zero returns a 10 ms window for EVERY index — not just
        // the tail. A source whose duration was never probed (a row the scan
        // has not reached, a legacy row) would then transcode 10 ms per
        // segment and playback would never advance past the first frame.
        let rate = FrameRate::from_mille(15_000);
        for total in [None, Some(0.0), Some(-1.0)] {
            for seg in [0, 1, 7] {
                let w = SegmentWindow::for_segment(seg, rate, total);
                let (nominal_start, nominal_dur) = segment_range(seg, rate);
                assert!(
                    (w.duration_seconds() - nominal_dur).abs() < 1e-6,
                    "total {total:?} seg {seg}: got {} s, want the nominal {nominal_dur} s",
                    w.duration_seconds()
                );
                assert!((w.start_seconds() - nominal_start).abs() < 1e-6);
            }
        }
    }
}
