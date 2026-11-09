//! Output downscaling (spec 035, ADR-0092).
//!
//! A transcode used to encode at the source's own resolution whatever the
//! client could show or afford. [`plan`] decides, from the source size, the
//! client's size bounds and the video bitrate the encode will run at, whether
//! the output is scaled down and to what. It never upscales, keeps the aspect
//! ratio, and returns even dimensions (4:2:0 chroma needs them).

use serde::{Deserialize, Serialize};

/// Smallest output edge. A bound below this is a malformed request, not a
/// wish for a thumbnail-sized stream.
pub const MIN_OUTPUT_EDGE: u32 = 64;

/// Bounds above this carry no information (no screen is that wide) and are
/// treated as absent.
pub const MAX_BOUND_EDGE: u32 = 8192;

/// The target frame size of a downscaled transcode. Only [`plan`] and
/// [`OutputSize::new`] build one, so a value in hand is always even, within
/// `MIN_OUTPUT_EDGE..=MAX_BOUND_EDGE` on both edges.
///
/// Deserialisation goes through [`OutputSize::new`], so a size read off the
/// wire is held to the same rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawSize", into = "RawSize")]
pub struct OutputSize {
    width: u32,
    height: u32,
}

#[derive(Serialize, Deserialize)]
struct RawSize {
    width: u32,
    height: u32,
}

impl From<OutputSize> for RawSize {
    fn from(s: OutputSize) -> Self {
        Self {
            width: s.width,
            height: s.height,
        }
    }
}

impl TryFrom<RawSize> for OutputSize {
    type Error = String;

    fn try_from(r: RawSize) -> Result<Self, String> {
        Self::new(r.width, r.height).ok_or_else(|| {
            format!(
                "invalid output size {}x{}: edges must be even and within {MIN_OUTPUT_EDGE}..={MAX_BOUND_EDGE}",
                r.width, r.height
            )
        })
    }
}

impl OutputSize {
    /// `None` unless both edges are even and within the sane range.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let ok = |e: u32| (MIN_OUTPUT_EDGE..=MAX_BOUND_EDGE).contains(&e) && e % 2 == 0;
        (ok(width) && ok(height)).then_some(Self { width, height })
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }

    /// CPU scaler, for the software filter chain (before the tonemap chain, so
    /// the float tonemap runs on the small frame).
    pub fn software_filter(self) -> String {
        format!("scale={}:{}", self.width, self.height)
    }

    /// VAAPI scaler for frames that stay on the GPU.
    pub fn vaapi_filter(self) -> String {
        format!("scale_vaapi=w={}:h={}", self.width, self.height)
    }

    /// NVENC route: scale on the GPU after NVDEC and hand NV12 frames straight
    /// to the encoder, which is why the format is named here.
    pub fn cuda_filter(self) -> String {
        format!("scale_cuda={}:{}:format=nv12", self.width, self.height)
    }

    /// libplacebo's own output-size options, for the GPU tonemap route: the
    /// scale happens in the same pass as the tonemap.
    pub fn libplacebo_options(self) -> String {
        format!("w={}:h={}", self.width, self.height)
    }
}

/// The client's bounding box. `0` and values above [`MAX_BOUND_EDGE`] read as
/// absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SizeLimits {
    max_width: Option<u32>,
    max_height: Option<u32>,
}

impl SizeLimits {
    pub fn new(max_width: Option<u32>, max_height: Option<u32>) -> Self {
        let sane = |v: Option<u32>| v.filter(|n| (1..=MAX_BOUND_EDGE).contains(n));
        Self {
            max_width: sane(max_width),
            max_height: sane(max_height),
        }
    }

    pub fn max_width(&self) -> Option<u32> {
        self.max_width
    }

    pub fn max_height(&self) -> Option<u32> {
        self.max_height
    }

    pub fn is_empty(&self) -> bool {
        self.max_width.is_none() && self.max_height.is_none()
    }
}

/// Why a transcode was scaled; the `reason` label of
/// `pharos_transcode_output_scaled_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleReason {
    /// The client's `MaxWidth` / `MaxHeight` bound the frame.
    MaxSize,
    /// The video bitrate is too low for the source resolution.
    Bitrate,
}

impl ScaleReason {
    pub const ALL: [Self; 2] = [Self::MaxSize, Self::Bitrate];

    pub const fn label(self) -> &'static str {
        match self {
            Self::MaxSize => "max_size",
            Self::Bitrate => "bitrate",
        }
    }
}

/// Lowest video bitrate (bps) at which H.264 holds up at a rung's height, from
/// the tallest rung down. A bitrate under a rung's floor drops the output to
/// the next rung. Deliberately low: it exists to stop a 4K encode at 1.5 Mbps
/// (which no one can watch), not to second-guess a client that chose its rate.
const BITRATE_LADDER: [(u32, u64); 5] = [
    (2160, 8_000_000),
    (1440, 5_000_000),
    (1080, 2_500_000),
    (720, 1_200_000),
    (480, 0),
];

/// The tallest output height a video bitrate supports.
pub fn height_for_bitrate(bps: u64) -> u32 {
    BITRATE_LADDER
        .iter()
        .find(|(_, floor)| bps >= *floor)
        .map(|(h, _)| *h)
        .unwrap_or(480)
}

/// Decide the output size for a source of `src_w` x `src_h`.
///
/// `align` is the multiple both edges are rounded DOWN to: 2 for plain video,
/// 4 for a packed 3D source, whose halves must each stay even. `None` means
/// encode at source size. The client's box wins over the bitrate when it is the
/// tighter bound.
pub fn plan(
    src_w: u32,
    src_h: u32,
    limits: SizeLimits,
    video_bitrate_bps: Option<u64>,
    align: u32,
) -> Option<(OutputSize, ScaleReason)> {
    plan_delivered(src_w, src_h, src_w, src_h, limits, video_bitrate_bps, align)
}

/// [`plan`] for a source whose DELIVERED frame is not the decoded one: a packed
/// 3D source downmixed to 2D crops one eye out (half the width of a full
/// side-by-side, half the height of a full top-and-bottom). The client's bounds
/// apply to the delivered frame, but the scale runs on the packed frame ahead
/// of the crop, so the factor is found on `delivered_*` and the size returned is
/// the packed frame scaled by it.
pub fn plan_delivered(
    src_w: u32,
    src_h: u32,
    delivered_w: u32,
    delivered_h: u32,
    limits: SizeLimits,
    video_bitrate_bps: Option<u64>,
    align: u32,
) -> Option<(OutputSize, ScaleReason)> {
    if src_w == 0 || src_h == 0 || delivered_w == 0 || delivered_h == 0 {
        return None;
    }
    let mut factor = 1.0_f64;
    let mut reason = None;
    if let Some(w) = limits.max_width {
        let f = f64::from(w) / f64::from(delivered_w);
        if f < factor {
            factor = f;
            reason = Some(ScaleReason::MaxSize);
        }
    }
    if let Some(h) = limits.max_height {
        let f = f64::from(h) / f64::from(delivered_h);
        if f < factor {
            factor = f;
            reason = Some(ScaleReason::MaxSize);
        }
    }
    if let Some(bps) = video_bitrate_bps {
        let f = f64::from(height_for_bitrate(bps)) / f64::from(delivered_h);
        if f < factor {
            factor = f;
            reason = Some(ScaleReason::Bitrate);
        }
    }
    let reason = reason?;
    // A tiny bound is floored so the shorter edge stays usable, keeping the
    // aspect ratio rather than clamping one edge alone.
    factor = factor.max(f64::from(MIN_OUTPUT_EDGE) / f64::from(delivered_w.min(delivered_h)));
    let align = align.max(2);
    let edge = |src: u32| {
        let scaled = (f64::from(src) * factor).floor() as u32;
        (scaled / align * align).max(align)
    };
    let (w, h) = (edge(src_w), edge(src_h));
    if w >= src_w || h >= src_h {
        // Alignment ate the whole reduction (or the source is smaller than the
        // grid): there is nothing to gain.
        return None;
    }
    OutputSize::new(w, h).map(|s| (s, reason))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn limits(w: Option<u32>, h: Option<u32>) -> SizeLimits {
        SizeLimits::new(w, h)
    }

    #[test]
    fn a_4k_source_bounded_to_1920_wide_scales_to_1080() {
        let (s, r) = plan(3840, 2160, limits(Some(1920), None), None, 2).unwrap();
        assert_eq!((s.width(), s.height()), (1920, 1080));
        assert_eq!(r, ScaleReason::MaxSize);
    }

    #[test]
    fn the_tighter_of_width_and_height_wins_and_aspect_holds() {
        let (s, _) = plan(3840, 2160, limits(Some(1920), Some(540)), None, 2).unwrap();
        assert_eq!((s.width(), s.height()), (960, 540));
        let (s, _) = plan(3840, 1608, limits(Some(1920), None), None, 2).unwrap();
        assert_eq!((s.width(), s.height()), (1920, 804));
    }

    #[test]
    fn never_upscales() {
        assert!(plan(1280, 720, limits(Some(1920), Some(1080)), None, 2).is_none());
        assert!(plan(1280, 720, limits(Some(1280), Some(720)), None, 2).is_none());
        assert!(plan(1280, 720, SizeLimits::default(), None, 2).is_none());
        assert!(plan(1280, 720, SizeLimits::default(), Some(10_000_000), 2).is_none());
    }

    #[test]
    fn edges_are_even_whatever_the_ratio() {
        let (s, _) = plan(1919, 1079, limits(Some(1001), None), None, 2).unwrap();
        assert_eq!(s.width() % 2, 0);
        assert_eq!(s.height() % 2, 0);
    }

    #[test]
    fn packed_3d_keeps_each_half_even() {
        let (s, _) = plan(3840, 1080, limits(Some(1950), None), None, 4).unwrap();
        assert_eq!(s.width() % 4, 0);
        assert_eq!(s.height() % 4, 0);
    }

    #[test]
    fn absurd_bounds_are_ignored_not_obeyed() {
        assert!(limits(Some(0), Some(0)).is_empty());
        assert!(limits(Some(100_000), None).is_empty());
        assert!(plan(3840, 2160, limits(Some(0), Some(100_000)), None, 2).is_none());
    }

    #[test]
    fn a_tiny_bound_floors_at_the_minimum_edge() {
        let (s, _) = plan(3840, 2160, limits(Some(8), None), None, 2).unwrap();
        assert_eq!((s.width(), s.height()), (112, 64));
    }

    #[test]
    fn a_starved_bitrate_picks_a_lower_rung() {
        for (bps, h) in [
            (12_000_000, None),
            (8_000_000, None),
            (6_000_000, Some(1440)),
            (3_000_000, Some(1080)),
            (1_500_000, Some(720)),
            (800_000, Some(480)),
        ] {
            let got = plan(3840, 2160, SizeLimits::default(), Some(bps), 2);
            assert_eq!(got.map(|(s, _)| s.height()), h, "{bps} bps");
            if let Some((_, r)) = got {
                assert_eq!(r, ScaleReason::Bitrate);
            }
        }
    }

    #[test]
    fn the_client_box_beats_a_looser_bitrate_rung() {
        let (s, r) = plan(3840, 2160, limits(None, Some(720)), Some(6_000_000), 2).unwrap();
        assert_eq!(s.height(), 720);
        assert_eq!(r, ScaleReason::MaxSize);
    }

    #[test]
    fn reason_labels_are_distinct() {
        let labels: Vec<_> = ScaleReason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels, ["max_size", "bitrate"]);
    }

    #[test]
    fn filters_name_the_size() {
        let s = OutputSize::new(1920, 1080).unwrap();
        assert_eq!(s.software_filter(), "scale=1920:1080");
        assert_eq!(s.vaapi_filter(), "scale_vaapi=w=1920:h=1080");
        assert_eq!(s.libplacebo_options(), "w=1920:h=1080");
        assert_eq!(s.cuda_filter(), "scale_cuda=1920:1080:format=nv12");
    }

    #[test]
    fn a_full_side_by_side_downmix_is_bounded_on_the_eye_not_the_packed_frame() {
        // 3840x1080 packed = two 1920x1080 eyes: already within MaxWidth=1920.
        let l = limits(Some(1920), None);
        assert!(plan_delivered(3840, 1080, 1920, 1080, l, None, 4).is_none());
        // 7680x2160 packed = two 3840x2160 eyes: the eye goes to 1920x1080, so
        // the packed frame goes to 3840x1080 and the crop halves it to the eye.
        let (s, _) = plan_delivered(7680, 2160, 3840, 2160, l, None, 4).unwrap();
        assert_eq!((s.width(), s.height()), (3840, 1080));
    }

    #[test]
    fn a_full_top_and_bottom_downmix_is_bounded_on_the_eye_not_the_packed_frame() {
        // 1920x2160 packed = two 1920x1080 eyes: within MaxHeight=1080.
        let l = limits(None, Some(1080));
        assert!(plan_delivered(1920, 2160, 1920, 1080, l, None, 4).is_none());
        // MaxHeight=540: the eye goes to 960x540, the packed frame to 960x1080.
        let l = limits(None, Some(540));
        let (s, _) = plan_delivered(1920, 2160, 1920, 1080, l, None, 4).unwrap();
        assert_eq!((s.width(), s.height()), (960, 1080));
    }

    #[test]
    fn a_wire_size_is_held_to_the_same_rules_as_new() {
        let ok: OutputSize = serde_json::from_str(r#"{"width":1920,"height":1080}"#).unwrap();
        assert_eq!((ok.width(), ok.height()), (1920, 1080));
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            r#"{"width":1920,"height":1080}"#
        );
        for bad in [
            r#"{"width":1921,"height":1080}"#,
            r#"{"width":32,"height":1080}"#,
            r#"{"width":16384,"height":1080}"#,
            r#"{"width":0,"height":0}"#,
        ] {
            assert!(serde_json::from_str::<OutputSize>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn odd_or_out_of_range_sizes_do_not_construct() {
        assert!(OutputSize::new(1921, 1080).is_none());
        assert!(OutputSize::new(32, 1080).is_none());
        assert!(OutputSize::new(16384, 1080).is_none());
    }
}
